//! The guest's speakers on Linux: a PulseAudio stream fed from the jitter buffer.
//!
//! The Linux counterpart of the WASAPI `audio_output`, compiled as that module
//! on Linux, with the same API so the guest's session code is the same on both.
//!
//! ## The device sets the pace
//!
//! The render thread pulls one period from [`JitterBuffer`] and writes it to
//! the sound server, whose write blocks while the server's queue is full. That
//! block is the clock: the thread runs exactly as fast as the device plays,
//! and the jitter buffer reconciles that with how the network delivers — the
//! same arrangement as the WASAPI event on Windows. What the server holds is
//! latency, so it is kept to [`DEVICE_TARGET_MS`], as on Windows.
//!
//! ## Which device
//!
//! The one chosen in Settings, or the default output, followed when it changes:
//! the sound server moves a stream that was not opened on a named device when
//! the default moves. A chosen device that is unplugged falls back to the
//! default until it returns, as on Windows. The render thread looks at this
//! about once a second, because a write cannot be interrupted to tell it.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;

use super::audio_render::JitterBuffer;
use super::encoder::LatencyPriority;
use super::pulse;
use crate::AudioDevice;

/// A running playback stream. Stopped when dropped.
pub struct AudioOutput {
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    /// Gain, as the bits of an `f32`, read by the render thread every period.
    volume: Arc<AtomicU32>,
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// `1.0f32`, as stored in [`AudioOutput::volume`].
const FULL_VOLUME: u32 = 0x3f80_0000;
const _: () = assert!(f32::from_bits(FULL_VOLUME) == 1.0);

/// How much sound to keep queued in the server, in milliseconds. Latency, so
/// as little as survives a late wake-up; the same budget as Windows.
const DEVICE_TARGET_MS: u32 = 15;

/// How much the render thread pulls and writes at a time.
const PERIOD_MS: u32 = 5;

/// How long to wait before trying the sound server again.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the render thread checks which device it should be on.
const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// The device chosen in Settings, by the sound server's name for it.
static CHOSEN: Mutex<Option<String>> = Mutex::new(None);

/// Play on this device, or on the default with `None`. Reaches sessions that
/// are already playing within [`RECHECK_EVERY`].
pub(crate) fn set_playback_device(id: Option<String>) {
    *CHOSEN.lock().unwrap_or_else(|e| e.into_inner()) = id;
}

/// The default device as the guest and the stats are told about it: by the name
/// the desktop's sound settings use. The sound server picks the real one; only
/// if it cannot be asked does this have to say no more than "the default".
pub(crate) fn default_device() -> AudioDevice {
    pulse::devices().into_iter().find(|device| device.default).unwrap_or(AudioDevice {
        id: String::new(),
        name: "Default output".to_string(),
        default: true,
    })
}

/// Which of `devices` a stream should open on, given what was chosen: the name
/// to open it with, or `None` for the default. A choice that is not plugged in
/// is the default for now.
pub(crate) fn resolved(chosen: Option<&str>, devices: &[AudioDevice]) -> Option<String> {
    chosen.filter(|id| devices.iter().any(|device| device.id == *id)).map(str::to_string)
}

/// Where this should be playing now, as a name to open and as a device to show.
fn target() -> (Option<String>, AudioDevice) {
    let chosen = CHOSEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let devices = pulse::devices();
    let open_on = resolved(chosen.as_deref(), &devices);
    let device = open_on
        .as_ref()
        .and_then(|id| devices.iter().find(|device| device.id == *id).cloned())
        .unwrap_or_else(default_device);
    (open_on, device)
}

impl AudioOutput {
    /// Start playing on the default output.
    ///
    /// Succeeds with no sound server to play on: the stream keeps trying, as
    /// the Windows one waits for a device. What does fail here is what no
    /// server would fix — no client library, no Opus.
    pub fn start(sample_rate: u32, channels: u16, priority: LatencyPriority) -> Result<Self> {
        anyhow::ensure!(pulse::available(), "libpulse-simple.so.0 is not installed, so there is no sound");
        let buffer = Arc::new(Mutex::new(JitterBuffer::new(sample_rate, channels, priority)?));
        let muted = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(FULL_VOLUME));
        // Opened here rather than on the thread, so `device()` answers
        // truthfully from the moment this returns, as on Windows. A server
        // that is not there yet is retried on the thread.
        let (open_on, on) = target();
        let first = open_playback(open_on.as_deref(), sample_rate, channels);
        let device = Arc::new(Mutex::new(first.as_ref().ok().map(|_| on.clone())));
        match &first {
            Ok(_) => info!("Playing on {}", on.name),
            Err(e) => warn!("No sound output yet ({e:#}) — trying again shortly"),
        }
        let stop = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new()
            .name("audio-render".into())
            .spawn({
                let buffer = Arc::clone(&buffer);
                let muted = Arc::clone(&muted);
                let volume = Arc::clone(&volume);
                let device = Arc::clone(&device);
                let stop = Arc::clone(&stop);
                let first = first.ok();
                move || render(first, open_on, sample_rate, channels, buffer, muted, volume, device, stop)
            })
            .context("could not start the audio render thread")?;

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

    /// Silence output without tearing the stream down, so unmuting resumes at
    /// the right place.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Playback gain, 0.0 to 1.0, applied here rather than to the system mixer
    /// so the remote machine can be quieter than everything else.
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
        // Joining is safe: a write blocks for at most the server's queue,
        // milliseconds, and the loop checks `stop` after every one.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_playback(device: Option<&str>, sample_rate: u32, channels: u16) -> Result<pulse::Stream> {
    pulse::Stream::playback("Remote desktop sound", device, sample_rate, channels, DEVICE_TARGET_MS, PERIOD_MS)
}

#[allow(clippy::too_many_arguments)]
fn render(
    mut stream: Option<pulse::Stream>,
    mut opened_on: Option<String>,
    sample_rate: u32,
    channels: u16,
    buffer: Arc<Mutex<JitterBuffer>>,
    muted: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
) {
    let set_device = |value: Option<AudioDevice>| {
        *device.lock().unwrap_or_else(|e| e.into_inner()) = value;
    };
    let period = (sample_rate * PERIOD_MS / 1000) as usize;
    let mut samples = Vec::with_capacity(period * channels as usize);
    // `start` has just tried; wait before the first retry if it failed.
    let mut retry_at = std::time::Instant::now() + if stream.is_some() { std::time::Duration::ZERO } else { RETRY_EVERY };
    let mut recheck_at = std::time::Instant::now() + RECHECK_EVERY;

    while !stop.load(Ordering::Relaxed) {
        // A different device was chosen, or the chosen one came or went. Named
        // again each time too, because the default can have moved under a
        // stream that is simply following it.
        if stream.is_some() && std::time::Instant::now() >= recheck_at {
            recheck_at = std::time::Instant::now() + RECHECK_EVERY;
            let (open_on, on) = target();
            if open_on != opened_on {
                stream = None;
                retry_at = std::time::Instant::now();
            } else {
                set_device(Some(on));
            }
        }

        if stream.is_none() {
            if std::time::Instant::now() < retry_at {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            let (open_on, on) = target();
            match open_playback(open_on.as_deref(), sample_rate, channels) {
                Ok(opened) => {
                    info!("Playing on {}", on.name);
                    set_device(Some(on));
                    opened_on = open_on;
                    stream = Some(opened);
                }
                Err(e) => {
                    warn!("No sound output ({e:#}) — trying again shortly");
                    set_device(None);
                    retry_at = std::time::Instant::now() + RETRY_EVERY;
                    continue;
                }
            }
        }

        samples.clear();
        buffer.lock().unwrap_or_else(|e| e.into_inner()).pull(period, &mut samples);
        let gain = if muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(volume.load(Ordering::Relaxed))
        };
        if gain != 1.0 {
            for sample in &mut samples {
                *sample *= gain;
            }
        }

        if let Some(open) = stream.as_mut() {
            if let Err(e) = open.write(&samples) {
                warn!("Sound output lost ({e:#}) — reopening");
                stream = None;
                set_device(None);
                retry_at = std::time::Instant::now() + std::time::Duration::from_millis(500);
            }
        }
    }
}
