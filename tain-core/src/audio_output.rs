//! The guest's speakers: a WASAPI render client fed from the jitter buffer.
//!
//! ## The device sets the pace, not the network
//!
//! A render client asks for exactly as many frames as it is about to consume,
//! on its own clock, forever. That is the opposite of how audio arrives, and
//! reconciling the two is [`super::audio_render::JitterBuffer`]'s job — this
//! module only has to make sure the device is never made to wait.
//!
//! Which means the render thread must never block on the network, never
//! allocate in the callback path, and never hand back a short buffer. It pulls
//! whatever the jitter buffer has, including the silence it manufactures, and
//! writes exactly what was asked for.
//!
//! ## Shared mode, not exclusive
//!
//! Exclusive mode would shave a few milliseconds and take the user's audio
//! device away from every other application on the machine — including, on the
//! host side of a loopback test, the thing being captured. Not a trade worth
//! making for a remote desktop.

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::audio_render::JitterBuffer;
use super::encoder::LatencyPriority;

/// A running playback stream. Stopped when dropped.
pub struct AudioOutput {
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    /// Gain, as the bits of an `f32`. An atomic because the render thread reads
    /// it on every callback and must never take a lock to do so.
    volume: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// `1.0f32`, as stored in [`AudioOutput::volume`].
const FULL_VOLUME: u32 = 0x3f80_0000;

const _: () = assert!(f32::from_bits(FULL_VOLUME) == 1.0);

impl AudioOutput {
    /// Open the default playback device and start feeding it.
    ///
    /// `priority` bounds the jitter buffer — see
    /// [`JitterBuffer`](super::audio_render::JitterBuffer). It is the guest's
    /// own setting, not the host's: this is the guest's latency and the guest's
    /// ears.
    pub fn start(sample_rate: u32, channels: u16, priority: LatencyPriority) -> Result<Self> {
        let buffer = Arc::new(Mutex::new(JitterBuffer::new(sample_rate, channels, priority)?));
        let muted = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(FULL_VOLUME));
        let stop = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("audio-render".into())
            .spawn({
                let buffer = Arc::clone(&buffer);
                let muted = Arc::clone(&muted);
                let volume = Arc::clone(&volume);
                let stop = Arc::clone(&stop);
                move || render_thread(sample_rate, channels, buffer, muted, volume, stop, ready_tx)
            })
            .context("could not start the audio render thread")?;

        ready_rx
            .recv()
            .context("the audio render thread stopped before it was ready")??;

        info!("Playing host audio at {sample_rate} Hz, {channels} channels");
        Ok(Self { buffer, muted, volume, stop, thread: Some(thread) })
    }

    /// Hand one received block to the jitter buffer.
    pub fn push(&self, packets: &[u8], first_frame: u64, frames: u32) -> Result<()> {
        self.buffer.lock().unwrap().push(packets, first_frame, frames)
    }

    /// Silence output without tearing the stream down.
    ///
    /// Muting rather than stopping keeps the timeline running, so unmuting
    /// resumes at the right place instead of replaying a stale backlog.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Playback gain, 0.0 to 1.0.
    ///
    /// Applied here rather than through the system mixer: this is the volume of
    /// *the remote machine's* sound, and the guest may well want that quieter
    /// than everything else it is playing. Touching the endpoint volume would
    /// turn down the local music too.
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn buffered_ms(&self) -> u32 {
        self.buffer.lock().unwrap().buffered_ms()
    }

    /// How much the jitter buffer is currently trying to hold.
    pub fn target_ms(&self) -> u32 {
        self.buffer.lock().unwrap().target_ms()
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How long to wait for the device before checking whether to stop.
const WAIT_TIMEOUT_MS: u32 = 200;

/// Device buffer to request, in 100-nanosecond units.
///
/// The *capacity* the engine allocates, not how full it is kept — see
/// `DEVICE_TARGET_MS`. Roomy enough that a descheduled render thread has
/// somewhere to catch up into.
const BUFFER_DURATION: i64 = 40 * 10_000;

/// How much audio to keep queued in the device, in milliseconds.
///
/// # Why this is not simply "as much as fits"
///
/// Whatever is sitting in the device buffer is latency: it was written before
/// the audio in front of it finishes playing. The first version primed the
/// *whole* buffer with silence and then topped it back up to full on every
/// callback, which pinned playback 40 ms behind for the life of the session and
/// was a large part of the 190-200 ms a real two-machine test measured.
///
/// 15 ms is a period and a half at the usual engine rate — enough that a late
/// wake-up does not underrun, and no more.
const DEVICE_TARGET_MS: u32 = 15;

#[allow(clippy::too_many_arguments)]
fn render_thread(
    sample_rate: u32,
    channels: u16,
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) {
    // SAFETY: paired with CoUninitialize below; every COM object stays on this
    // thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(anyhow::anyhow!("CoInitializeEx failed: {com:?}")));
        return;
    }

    match open_render(sample_rate, channels) {
        Ok((client, render, event, device_frames)) => {
            if ready.send(Ok(())).is_err() {
                // SAFETY: balances the CoInitializeEx above.
                unsafe { CoUninitialize() };
                return;
            }
            run(
                &client, &render, event, device_frames, sample_rate, channels, &buffer,
                &muted, &volume, &stop,
            );
            // SAFETY: the event was created here and is not used again.
            unsafe {
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
    // SAFETY: balances the CoInitializeEx above.
    unsafe { CoUninitialize() };
}

/// Open the default render endpoint at the stream's format.
fn open_render(
    sample_rate: u32,
    channels: u16,
) -> Result<(IAudioClient, IAudioRenderClient, HANDLE, u32)> {
    // SAFETY: standard WASAPI activation; every returned pointer is checked and
    // the format is freed before returning.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("could not create the audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default playback device")?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate a render client")?;

        // Ask for the stream's own format rather than the endpoint's. The two
        // rarely match — the host may mix at 44.1 kHz while this machine runs
        // at 48 — and AUTOCONVERTPCM makes the audio engine resample instead of
        // this code carrying a resampler it would get subtly wrong.
        let mix = client.GetMixFormat().context("could not read the mix format")?;
        anyhow::ensure!(!mix.is_null(), "GetMixFormat returned nothing");
        let mut format = *mix;
        CoTaskMemFree(Some(mix as *const _));

        format.nChannels = channels;
        format.nSamplesPerSec = sample_rate;
        format.wBitsPerSample = 32;
        format.nBlockAlign = channels * 4;
        format.nAvgBytesPerSec = sample_rate * channels as u32 * 4;
        format.cbSize = 0;
        // Plain IEEE float, not EXTENSIBLE: the extended header carries a
        // channel mask this does not fill in, and a mismatched one is refused.
        format.wFormatTag = 0x0003;

        let event = CreateEventW(None, false, false, None)
            .context("could not create the render event")?;

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                &format,
                None,
            )
            .context("the audio engine refused a render client")?;

        client.SetEventHandle(event).context("could not attach the render event")?;
        let device_frames = client.GetBufferSize().context("could not read the buffer size")?;
        let render: IAudioRenderClient =
            client.GetService().context("could not get a render client")?;

        // Primed with one target's worth of silence, not the whole buffer.
        // Enough that the first callback has something to play; filling it
        // completely would put every sample that followed 40 ms behind.
        let prime = (sample_rate * DEVICE_TARGET_MS / 1000).min(device_frames);
        let data = render.GetBuffer(prime)?;
        std::ptr::write_bytes(data, 0, prime as usize * channels as usize * 4);
        render.ReleaseBuffer(prime, 0)?;

        client.Start().context("could not start playback")?;
        Ok((client, render, event, device_frames))
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    event: HANDLE,
    device_frames: u32,
    sample_rate: u32,
    channels: u16,
    buffer: &Mutex<JitterBuffer>,
    muted: &AtomicBool,
    volume: &AtomicU32,
    stop: &AtomicBool,
) {
    // Reused across callbacks: allocating in the render path is how a stream
    // develops an occasional click nobody can reproduce.
    let mut pcm: Vec<f32> = Vec::with_capacity(device_frames as usize * channels as usize);
    // Where the buffer is drained to while muted, for the same reason.
    let mut scratch: Vec<f32> = Vec::with_capacity(pcm.capacity());

    while !stop.load(Ordering::Relaxed) {
        // SAFETY: waiting on an event this thread owns.
        if unsafe { WaitForSingleObject(event, WAIT_TIMEOUT_MS) } != WAIT_OBJECT_0 {
            continue;
        }

        // SAFETY: reading how much of our own buffer the device has consumed.
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(used) => used,
            Err(e) => {
                warn!("Audio playback stopped: {e}");
                return;
            }
        };
        // Top up to the target, not to the brim. Anything beyond the target is
        // audio queued behind audio — latency with no benefit.
        let target = (sample_rate * DEVICE_TARGET_MS / 1000).min(device_frames);
        let free = target.saturating_sub(padding).min(device_frames - padding);
        if free == 0 {
            continue;
        }

        // Pull first, then take the device buffer: holding a GetBuffer across a
        // lock the network side also wants is how a render thread stalls.
        if muted.load(Ordering::Relaxed) {
            pcm.clear();
            pcm.resize(free as usize * channels as usize, 0.0);
            // Still drained, so unmuting resumes at the live position rather
            // than replaying however much accumulated while silent.
            buffer.lock().unwrap().pull(free as usize, &mut scratch);
        } else {
            buffer.lock().unwrap().pull(free as usize, &mut pcm);
            // Linear gain on float samples, which is what the device wants
            // anyway — no conversion, no clipping, and it costs one multiply per
            // sample. Skipped entirely at full volume so the common case is
            // exactly what it was before.
            let gain = f32::from_bits(volume.load(Ordering::Relaxed));
            if gain != 1.0 {
                for sample in pcm.iter_mut() {
                    *sample *= gain;
                }
            }
        }

        // SAFETY: `free` frames were reported available; the buffer is written
        // in full and released exactly once on both paths.
        unsafe {
            let data = match render.GetBuffer(free) {
                Ok(data) => data,
                Err(e) => {
                    warn!("Audio playback stopped: {e}");
                    return;
                }
            };
            let wanted = free as usize * channels as usize;
            // `pull` guarantees a full buffer, but the device's word is final:
            // writing more than it offered would corrupt memory it owns.
            let n = pcm.len().min(wanted);
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), data as *mut f32, n);
            if n < wanted {
                std::ptr::write_bytes((data as *mut f32).add(n), 0, wanted - n);
            }
            if let Err(e) = render.ReleaseBuffer(free, 0) {
                warn!("Audio playback stopped: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens the real default playback device and plays silence briefly.
    #[test]
    #[ignore = "touches the real default playback device"]
    fn playback_starts_and_stops_cleanly() {
        let Ok(output) = AudioOutput::start(48_000, 2, LatencyPriority::BALANCED) else {
            eprintln!("no playback device — skipping");
            return;
        };
        assert_eq!(output.buffered_ms(), 0, "nothing pushed yet");
        output.set_muted(true);
        std::thread::sleep(std::time::Duration::from_millis(200));
        // Dropping must join the render thread rather than leaving it running.
        drop(output);
    }

    /// Audible end to end: encode a tone, play it, and hear it.
    #[test]
    #[ignore = "plays sound on the real default device"]
    fn a_tone_makes_it_all_the_way_to_the_speakers() {
        use crate::audio_codec::{OpusEncoder, DEFAULT_BITRATE, SAMPLE_RATE};

        let Ok(mut encoder) = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE) else {
            eprintln!("no libopus — skipping");
            return;
        };
        let Ok(output) = AudioOutput::start(SAMPLE_RATE, 2, LatencyPriority::BALANCED) else {
            eprintln!("no playback device — skipping");
            return;
        };

        // One second of 440 Hz, fed in real time as the network would.
        let mut frame = 0u64;
        for _ in 0..100 {
            let pcm: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let t = (frame as usize + i) as f32 / SAMPLE_RATE as f32;
                    let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
                    [v, v]
                })
                .collect();
            let blob = encoder.encode(&pcm).expect("encode").to_vec();
            if !blob.is_empty() {
                output.push(&blob, frame, 480).expect("push");
                frame += 480;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        println!("played {frame} frames, {} ms still buffered", output.buffered_ms());
    }
}
