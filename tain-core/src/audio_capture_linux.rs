//! System audio capture on Linux: what the default output is playing.
//!
//! The Linux counterpart of the WASAPI `audio_capture`, compiled as that
//! module on Linux with the part of its API the host's audio pipeline uses, so
//! `network::spawn_audio_pipeline` is the same code on both.
//!
//! ## What is captured
//!
//! The default output's *monitor*, `@DEFAULT_MONITOR@`: everything the machine
//! plays, after mixing, which is what WASAPI loopback captures on Windows. The
//! sound server moves the stream when the default output changes. A sink
//! whose monitor is being recorded is in use, so it does not suspend when
//! nothing is playing, and the monitor delivers silence rather than stopping —
//! the gapless stream the guest's jitter buffer needs.
//!
//! ## What is not, yet
//!
//! Leaving chosen applications out of the stream. PulseAudio's monitor is the
//! mix, and the mix cannot be unmixed; doing it means building the capture from
//! the applications' own streams instead, which on PipeWire is a graph of
//! links rather than one stream. Asked for, it is logged and the whole mix is
//! sent. Choosing the device is not wired either.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;

use super::pulse;
use crate::AudioDevice;

/// What the endpoint is running at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    /// Frames in `duration` at this rate, rounded down.
    pub fn frames(&self, duration: std::time::Duration) -> usize {
        (duration.as_secs_f64() * self.sample_rate as f64) as usize
    }
}

/// What every capture delivers: 48 kHz stereo, as on Windows. The sound server
/// converts whatever the device runs at.
pub const STREAM_FORMAT: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

/// One block of captured audio, interleaved float32.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// Frames captured before this block: the presentation clock, from the
    /// running total rather than arrival time.
    pub position: u64,
    /// Whether every sample in the block is zero.
    pub silent: bool,
}

/// An application playing sound. Never listed on Linux yet; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSession {
    pub pid: u32,
    pub name: String,
}

/// Applications playing sound, for an exclusion picker. Empty on Linux, which
/// is the honest answer while nothing can be excluded.
pub fn audio_sessions() -> Vec<AudioSession> {
    Vec::new()
}

/// What to exclude for these processes: as given, since nothing is excluded.
pub fn exclusion_targets(pids: &[u32]) -> Vec<u32> {
    pids.to_vec()
}

/// How much each read waits for. The block a guest's buffer counts in, and the
/// floor on how late a captured sound can be sent.
const BLOCK_MS: u32 = 10;

/// How long to wait before trying the sound server again.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// A capture of the default output, for one session.
///
/// Each session has its own: the sound server serves any number of readers of
/// one monitor, which is the sharing the Windows type has to do by hand.
pub struct SharedCapture {
    chunks: std::sync::mpsc::Receiver<AudioChunk>,
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
}

impl SharedCapture {
    /// Start capturing what the default output plays.
    ///
    /// `excluding` cannot be honoured yet, and says so. Fails only when there
    /// is no client library to talk to a sound server with; a server that is
    /// not running yet is waited for.
    pub fn join(excluding: &[u32]) -> Result<Self> {
        anyhow::ensure!(pulse::available(), "libpulse-simple.so.0 is not installed, so there is no sound");
        if !excluding.is_empty() {
            info!("Leaving applications out of the sound is not supported on Linux yet — sending everything");
        }
        let (tx, chunks) = std::sync::mpsc::sync_channel(200);
        let device = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn({
                let device = Arc::clone(&device);
                let stop = Arc::clone(&stop);
                move || capture(tx, device, stop)
            })
            .context("could not start the audio capture thread")?;
        Ok(Self { chunks, device, stop })
    }

    pub fn format(&self) -> AudioFormat {
        STREAM_FORMAT
    }

    /// The device being captured, while a stream is open.
    pub fn device(&self) -> Option<AudioDevice> {
        self.device.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The captured blocks, in order. Closed when the capture ends.
    pub fn chunks(&self) -> &std::sync::mpsc::Receiver<AudioChunk> {
        &self.chunks
    }
}

impl Drop for SharedCapture {
    fn drop(&mut self) {
        // Not joined: a read waits on the sound server, and a server that has
        // gone quiet must not hold up the end of a session. The thread sees the
        // flag after its next block and closes its stream then.
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn capture(
    tx: std::sync::mpsc::SyncSender<AudioChunk>,
    device: Arc<Mutex<Option<AudioDevice>>>,
    stop: Arc<AtomicBool>,
) {
    let set_device = |value: Option<AudioDevice>| {
        *device.lock().unwrap_or_else(|e| e.into_inner()) = value;
    };
    let format = STREAM_FORMAT;
    let block = (format.sample_rate * BLOCK_MS / 1000) as usize * format.channels as usize;
    let mut position: u64 = 0;
    let mut stream: Option<pulse::Stream> = None;

    while !stop.load(Ordering::Relaxed) {
        let open = match stream.as_mut() {
            Some(open) => open,
            None => match pulse::Stream::record(
                "Sound for remote guests",
                "@DEFAULT_MONITOR@",
                format.sample_rate,
                format.channels,
                BLOCK_MS,
            ) {
                Ok(opened) => {
                    info!("Capturing what the default output plays");
                    set_device(Some(crate::audio_output::default_device()));
                    stream.insert(opened)
                }
                Err(e) => {
                    warn!("No sound to capture ({e:#}) — trying again shortly");
                    set_device(None);
                    std::thread::sleep(RETRY_EVERY);
                    continue;
                }
            },
        };

        let mut samples = vec![0f32; block];
        if let Err(e) = open.read(&mut samples) {
            warn!("Sound capture lost ({e:#}) — reopening");
            stream = None;
            set_device(None);
            continue;
        }
        let frames = (block / format.channels as usize) as u64;
        let silent = samples.iter().all(|s| *s == 0.0);
        let chunk = AudioChunk { samples, position, silent };
        position += frames;
        match tx.try_send(chunk) {
            Ok(()) => {}
            // A listener this far behind is better served by losing a block
            // than by queuing seconds of sound in front of the next one.
            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held by each test that plays sound: run together, one test's tone is
    /// the other's "silence".
    static SOUND_SERVER: Mutex<()> = Mutex::new(());

    /// Peak amplitude over the chunks received in `window`, after skipping any
    /// queued before `start` was called.
    fn loudest(capture: &SharedCapture, window: std::time::Duration) -> f32 {
        let until = std::time::Instant::now() + window;
        let mut peak = 0f32;
        while std::time::Instant::now() < until {
            if let Ok(chunk) = capture.chunks().recv_timeout(std::time::Duration::from_millis(100)) {
                peak = chunk.samples.iter().fold(peak, |p, s| p.max(s.abs()));
            }
        }
        peak
    }

    fn sine(frames: usize, start: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    /// What the default output plays comes back through the capture.
    ///
    /// Needs a sound server. Run with
    /// `cargo test -p tain-core -- --ignored what_is_played`.
    #[test]
    #[ignore = "needs a running sound server"]
    fn what_is_played_is_captured() {
        let _turn = SOUND_SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let capture = SharedCapture::join(&[]).expect("capture");
        // Silence first: nothing is playing yet.
        let quiet = loudest(&capture, std::time::Duration::from_millis(500));
        assert!(quiet < 0.01, "captured {quiet} with nothing playing");
        assert!(capture.device().is_some(), "an open capture names its device");

        let player = std::thread::spawn(|| {
            let mut out = pulse::Stream::playback("test tone", 48_000, 2, 15, 5).expect("playback");
            for block in 0..200 {
                out.write(&sine(240, block * 240)).expect("write");
            }
        });
        let heard = loudest(&capture, std::time::Duration::from_millis(800));
        player.join().unwrap();
        assert!(heard > 0.2, "played a 0.5 tone and captured a peak of {heard}");
    }

    /// The guest's whole path short of the network: Opus packets pushed into
    /// `AudioOutput` are played, and the capture hears them.
    #[test]
    #[ignore = "needs a running sound server"]
    fn a_guest_plays_what_it_is_sent() {
        use crate::audio_codec::OpusEncoder;
        use crate::audio_output::AudioOutput;
        use crate::encoder::LatencyPriority;

        let _turn = SOUND_SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let capture = SharedCapture::join(&[]).expect("capture");
        let output = AudioOutput::start(48_000, 2, LatencyPriority::default()).expect("output");
        let mut encoder = OpusEncoder::new(48_000, 2, 160_000).expect("opus");
        // A sink that was idle can take a while to drop to the latency asked
        // for — PulseAudio's null sink idles in two-second blocks — and a real
        // session is long past that by the time it matters. Let it settle.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        while capture.chunks().try_recv().is_ok() {}

        let feeder = std::thread::spawn(move || {
            let mut position = 0u64;
            for _ in 0..200 {
                let samples = sine(480, position as usize);
                let packets = encoder.encode(&samples).expect("encode").to_vec();
                if !packets.is_empty() {
                    output.push(&packets, position, 480).expect("push");
                }
                position += 480;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            output
        });
        let heard = loudest(&capture, std::time::Duration::from_millis(2000));
        let output = feeder.join().unwrap();
        assert!(output.device().is_some(), "playing output names its device");
        assert!(heard > 0.1, "sent a 0.5 tone and the speakers peaked at {heard}");
    }
}
