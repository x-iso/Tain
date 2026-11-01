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
//! The default output, followed when it changes: the sound server moves a
//! stream that was not opened on a named device when the default moves.
//! Choosing a device in Settings is not wired on Linux yet, so the picker
//! offers only the default.

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

/// The device as the picker and the stats show it. The sound server picks the
/// real one; this says only that it is the default.
pub(crate) fn default_device() -> AudioDevice {
    AudioDevice { id: String::new(), name: "Default output".to_string(), default: true }
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
        let device = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new()
            .name("audio-render".into())
            .spawn({
                let buffer = Arc::clone(&buffer);
                let muted = Arc::clone(&muted);
                let volume = Arc::clone(&volume);
                let device = Arc::clone(&device);
                let stop = Arc::clone(&stop);
                move || render(sample_rate, channels, buffer, muted, volume, device, stop)
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

fn render(
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
    let mut stream: Option<pulse::Stream> = None;
    let mut retry_at = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if stream.is_none() {
            if std::time::Instant::now() < retry_at {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            match pulse::Stream::playback("Remote desktop sound", sample_rate, channels, DEVICE_TARGET_MS, PERIOD_MS) {
                Ok(opened) => {
                    info!("Playing on the default output");
                    set_device(Some(default_device()));
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
