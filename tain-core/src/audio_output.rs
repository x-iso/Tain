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
//!
//! ## Which device
//!
//! The one chosen with [`super::audio_device::set_playback_device`], or the
//! default — and it moves when either changes or the device goes away. With no
//! device at all the stream keeps running and waits for one, so a guest whose
//! headphones were unplugged when the session started hears the host as soon as
//! they are back. See [`super::audio_device`].

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDevice, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::audio_device::{resolve, should_move, DeviceWatch, PLAYBACK};
use super::audio_render::JitterBuffer;
use super::encoder::LatencyPriority;
use crate::AudioDevice;

/// A running playback stream. Stopped when dropped.
pub struct AudioOutput {
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    /// Gain, as the bits of an `f32`. An atomic because the render thread reads
    /// it on every callback and must never take a lock to do so.
    volume: Arc<AtomicU32>,
    /// What is playing the sound right now, or `None` while there is nothing to
    /// play it on. Written by the render thread when it opens or loses one.
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// `1.0f32`, as stored in [`AudioOutput::volume`].
const FULL_VOLUME: u32 = 0x3f80_0000;

const _: () = assert!(f32::from_bits(FULL_VOLUME) == 1.0);

impl AudioOutput {
    /// Start playing, on the chosen device or the default.
    ///
    /// Succeeds without any device to play on: the stream waits for one rather
    /// than giving up for the whole session. What does fail here is what no
    /// device would fix — no decoder, no COM.
    ///
    /// `priority` bounds the jitter buffer — see
    /// [`JitterBuffer`](super::audio_render::JitterBuffer). It is the guest's
    /// own setting, not the host's: this is the guest's latency and the guest's
    /// ears.
    pub fn start(sample_rate: u32, channels: u16, priority: LatencyPriority) -> Result<Self> {
        let buffer = Arc::new(Mutex::new(JitterBuffer::new(sample_rate, channels, priority)?));
        let muted = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(FULL_VOLUME));
        let device = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("audio-render".into())
            .spawn({
                let stream = Stream {
                    sample_rate,
                    channels,
                    buffer: Arc::clone(&buffer),
                    muted: Arc::clone(&muted),
                    volume: Arc::clone(&volume),
                    device: Arc::clone(&device),
                    stop: Arc::clone(&stop),
                };
                move || render_thread(stream, ready_tx)
            })
            .context("could not start the audio render thread")?;

        ready_rx
            .recv()
            .context("the audio render thread stopped before it was ready")??;

        info!("Playing host audio at {sample_rate} Hz, {channels} channels");
        Ok(Self { buffer, muted, volume, device, stop, thread: Some(thread) })
    }

    /// The device the sound is playing on, or `None` while there is none.
    pub fn device(&self) -> Option<AudioDevice> {
        self.device.lock().unwrap_or_else(|e| e.into_inner()).clone()
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

/// How long to wait before looking for a device again, when there was none and
/// nothing has said one arrived.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// How long to wait before reopening a device that failed straight away.
const LOST_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Everything the render thread shares with the [`AudioOutput`] that owns it.
struct Stream {
    sample_rate: u32,
    channels: u16,
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
}

impl Stream {
    fn set_device(&self, device: Option<AudioDevice>) {
        *self.device.lock().unwrap_or_else(|e| e.into_inner()) = device;
    }
}

fn render_thread(stream: Stream, ready: std::sync::mpsc::Sender<Result<()>>) {
    // First, so the CoUninitialize below can never be the one that takes the
    // process's apartment away from a session still using it. See
    // `audio_device::ensure_mta`.
    super::audio_device::ensure_mta();
    // SAFETY: paired with CoUninitialize below; every COM object stays on this
    // thread, and `play` has dropped all of them by the time it returns.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(anyhow::anyhow!("CoInitializeEx failed: {com:?}")));
        return;
    }
    play(&stream, &ready);
    // SAFETY: balances the CoInitializeEx above.
    unsafe { CoUninitialize() };
}

/// Play on whichever device is right, for as long as the stream runs.
///
/// The one chosen in settings while it is plugged in, the default otherwise —
/// and when the default changes, the device goes away, or the choice is
/// changed, playback moves. The jitter buffer is the stream's, not the
/// device's, so moving costs the few milliseconds of reopening and nothing
/// that was already received.
fn play(stream: &Stream, ready: &std::sync::mpsc::Sender<Result<()>>) {
    let enumerator = match super::audio_device::enumerator() {
        Ok(enumerator) => enumerator,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Without notifications a lost device is still noticed — the stream fails —
    // but a new default would not be followed until then.
    let watch = DeviceWatch::register(&enumerator)
        .map_err(|e| warn!("Audio playback will not follow device changes: {e:#}"))
        .ok();
    let changed = || watch.as_ref().is_some_and(DeviceWatch::take);

    // Ready whether or not there is a device: see `AudioOutput::start`.
    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut said_missing = false;
    while !stream.stop.load(Ordering::Relaxed) {
        let seen = PLAYBACK.generation();
        let chosen = PLAYBACK.chosen();
        let opened = resolve(&enumerator, chosen.as_deref()).and_then(|(device, about, how)| {
            open_render(&device, stream.sample_rate, stream.channels)
                .map(|opened| (opened, about, how))
        });

        let ((client, render, event, device_frames), about, how) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                // Once per absence. The old message said the session would
                // stay silent, which was true and is not any more.
                if !said_missing {
                    warn!("No audio output ({e:#}) — waiting for a playback device");
                    said_missing = true;
                }
                stream.set_device(None);
                // Anything happening to any device is worth another look, and
                // so is time passing, in case a notification went astray.
                let retry = || changed() || PLAYBACK.generation() != seen;
                idle(stream, RETRY_EVERY, &retry);
                continue;
            }
        };

        info!("Playing host audio on {} — {}", about.name, how.describe());
        said_missing = false;
        stream.set_device(Some(about.clone()));
        let generation = std::cell::Cell::new(seen);
        let moved = || should_move(&enumerator, changed(), &PLAYBACK, &generation, &about.id);
        let opened_at = std::time::Instant::now();
        let ended = run(&client, &render, event, device_frames, stream, &moved);
        // SAFETY: the event was created for this client and is not used again.
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(event);
        }

        match ended {
            Ended::Stopped => break,
            Ended::Moved => {}
            Ended::Lost => {
                info!("Stopped playing on {} — looking for another device", about.name);
                // A device that fails as soon as it opens would otherwise be
                // reopened as fast as the loop can turn.
                if opened_at.elapsed() < std::time::Duration::from_secs(1) {
                    idle(stream, LOST_PAUSE, &|| false);
                }
            }
        }
    }
}

/// Keep the jitter buffer moving in real time while nothing is playing it.
///
/// The network goes on delivering whether or not there is a device, and the
/// buffer only sheds a backlog when it is pulled from. Left alone it would grow
/// for as long as the device was missing — without limit — and then play all
/// of it late.
fn idle(stream: &Stream, limit: std::time::Duration, until: &dyn Fn() -> bool) {
    let mut scratch = Vec::new();
    let started = std::time::Instant::now();
    let mut pulled = 0u64;
    while !stream.stop.load(Ordering::Relaxed) && started.elapsed() < limit && !until() {
        std::thread::sleep(std::time::Duration::from_millis(DEVICE_TARGET_MS as u64));
        let due = (started.elapsed().as_secs_f64() * stream.sample_rate as f64) as u64;
        let frames = due.saturating_sub(pulled) as usize;
        if frames == 0 {
            continue;
        }
        stream.buffer.lock().unwrap().pull(frames, &mut scratch);
        pulled += frames as u64;
    }
}

/// Why [`run`] returned.
enum Ended {
    /// The stream is being shut down.
    Stopped,
    /// A different device should be playing now.
    Moved,
    /// The device stopped working: unplugged, disabled, or its format changed
    /// under the stream.
    Lost,
}

/// Open a playback device at the stream's format.
fn open_render(
    device: &IMMDevice,
    sample_rate: u32,
    channels: u16,
) -> Result<(IAudioClient, IAudioRenderClient, HANDLE, u32)> {
    // SAFETY: standard WASAPI activation; every returned pointer is checked, the
    // format is freed before returning, and the event is closed on every path
    // that does not hand it back.
    unsafe {
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

        let event = CreateEventW(None, false, false, None)
            .context("could not create the render event")?;
        let started = (|| -> Result<(IAudioRenderClient, u32)> {
            client.SetEventHandle(event).context("could not attach the render event")?;
            let device_frames =
                client.GetBufferSize().context("could not read the buffer size")?;
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
            Ok((render, device_frames))
        })();
        match started {
            Ok((render, device_frames)) => Ok((client, render, event, device_frames)),
            Err(e) => {
                let _ = CloseHandle(event);
                Err(e)
            }
        }
    }
}

fn run(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    event: HANDLE,
    device_frames: u32,
    stream: &Stream,
    moved: &dyn Fn() -> bool,
) -> Ended {
    let Stream { sample_rate, channels, buffer, muted, volume, stop, .. } = stream;
    let (sample_rate, channels) = (*sample_rate, *channels);
    // Reused across callbacks: allocating in the render path is how a stream
    // develops an occasional click nobody can reproduce.
    let mut pcm: Vec<f32> = Vec::with_capacity(device_frames as usize * channels as usize);
    // Where the buffer is drained to while muted, for the same reason.
    let mut scratch: Vec<f32> = Vec::with_capacity(pcm.capacity());

    while !stop.load(Ordering::Relaxed) {
        // Once a period: two atomic loads unless something has happened.
        if moved() {
            return Ended::Moved;
        }

        // A timeout is not skipped. A device that has gone away can stop
        // signalling, and the padding read below is what says so — skipping
        // it would leave a stream waiting on a dead device for as long as
        // nothing else happened to notice. Topping up after a timeout is
        // harmless: the padding says how much room there really is.
        // SAFETY: waiting on an event this thread owns.
        unsafe { WaitForSingleObject(event, WAIT_TIMEOUT_MS) };

        // SAFETY: reading how much of our own buffer the device has consumed.
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(used) => used,
            Err(e) => {
                // Not a warning any more: the caller finds another device.
                info!("Audio playback lost its device: {e}");
                return Ended::Lost;
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
                    info!("Audio playback lost its device: {e}");
                    return Ended::Lost;
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
                info!("Audio playback lost its device: {e}");
                return Ended::Lost;
            }
        }
    }
    Ended::Stopped
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

    /// Choosing another device moves playback that is already running, and
    /// choosing the default again brings it back.
    ///
    /// Needs two playback devices, and leaves the system default alone: the
    /// move is made by choosing, which is Tain's own setting. Muted, so the
    /// only thing played on either is silence.
    #[test]
    #[ignore = "touches the real playback devices"]
    fn choosing_another_device_moves_playback_that_is_running() {
        // SAFETY: COM has to be up for the enumeration; this is a test thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let devices = crate::audio_device::playback_devices();
        let Some(other) = devices.iter().find(|d| !d.default).cloned() else {
            eprintln!("only one playback device — skipping");
            return;
        };
        crate::audio_device::set_playback_device(None);
        let output = AudioOutput::start(48_000, 2, LatencyPriority::BALANCED).expect("output");
        output.set_muted(true);

        // Opening is the render thread's job and happens after `start`.
        let settle = |want: &dyn Fn(Option<&AudioDevice>) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline && !want(output.device().as_ref()) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            output.device()
        };

        let before = settle(&|d| d.is_some()).expect("playing on the default");
        assert!(before.default, "started on {} rather than the default", before.name);

        crate::audio_device::set_playback_device(Some(other.id.clone()));
        let moved = settle(&|d| d.is_some_and(|d| d.id == other.id));
        // Put back before asserting, so a failure does not leave the choice set.
        crate::audio_device::set_playback_device(None);
        let back = settle(&|d| d.is_some_and(|d| d.id == before.id));

        assert_eq!(moved.map(|d| d.id), Some(other.id.clone()), "did not move to {}", other.name);
        assert_eq!(back.map(|d| d.id), Some(before.id.clone()), "did not come back to {}", before.name);
        println!("moved {} -> {} -> {}", before.name, other.name, before.name);
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
