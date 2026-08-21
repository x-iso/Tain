//! Playing the host's audio on the guest, through WASAPI.
//!
//! ## The jitter buffer is the whole problem
//!
//! Audio has to be handed to the device at exactly the rate the device consumes
//! it, forever, with no gaps. The network delivers in bursts and sometimes not
//! at all. Everything here exists to turn the second into the first.
//!
//! A buffer that is too short underruns on the first hiccup and clicks; one that
//! is too long is latency the user hears as the sound lagging the picture. The
//! target here starts small and grows only when the network proves it needs to
//! — see [`JitterBuffer`].
//!
//! ## Sound is corrected towards the picture, never the other way round
//!
//! A media player delays video until the audio catches up, because a constant
//! offset is invisible and lip sync is not. A remote desktop is the opposite:
//! the user is *driving* it, and holding video back to match an audio buffer
//! would add that buffer to the delay between moving the mouse and seeing it
//! move — the one thing that has to stay small.
//!
//! So video is always presented the moment it arrives, and any offset is taken
//! out of the audio side by keeping this buffer short and trimming it when it
//! drifts. The guest measures and reports the result; see `note_audio`.
//!
//! ## Why underrun is filled rather than skipped
//!
//! When there is nothing to play, something is played anyway: the concealment
//! from [`super::audio_codec::OpusDecoder::conceal`], or silence once that has
//! faded. Handing the device a short buffer instead would make it repeat
//! whatever was there before, which is the loudest possible way to fail.

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::{Context, Result};
use std::collections::VecDeque;
use tracing::*;

use super::audio_codec::OpusDecoder;
use super::encoder::LatencyPriority;

/// How much a single underrun adds to the target, in milliseconds.
///
/// # Why it grows in a step rather than smoothly
///
/// An underrun is not a hint that the buffer is slightly too small. It is proof
/// that the link's jitter exceeded it, and the next burst of jitter will be
/// about the same size — so creeping up a millisecond at a time means
/// underrunning several more times on the way to a size that would have worked.
/// Each underrun also costs a full refill of silence, which is the audible part.
///
/// Growing by a fifth of a typical arrival gap gets there in two or three
/// hiccups instead of thirty.
const GROW_ON_UNDERRUN_MS: u32 = 20;

/// How long the buffer must run clean before it gives a millisecond back.
///
/// Slow on purpose, and much slower than it grows. A buffer that shrank as
/// eagerly as it grew would spend a jittery session oscillating between too
/// small and just right, underrunning every time it tried.
const DECAY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// How much is given back each time. One Opus packet.
const DECAY_STEP_MS: u32 = 10;

/// Decoded audio waiting to be played, with enough history to tell when
/// something is missing.
///
/// # The buffer sizes itself
///
/// It used to hold a fixed 20 ms — two Opus packets — which is right for a link
/// on the same desk and hopeless on one with a hundred milliseconds of distance
/// in it. Every jitter spike past 20 ms underran, and an underrun stops playback
/// until the buffer refills, so the failure is not a click but a gap followed by
/// another gap. Reported as audio that "stutters on its own".
///
/// Now it starts at [`LatencyPriority::audio_buffer_floor_ms`], grows whenever
/// it is proved too small, and decays back down when it has not been. The bounds
/// come from the quality-versus-latency slider, because that is exactly what the
/// trade is.
pub struct JitterBuffer {
    decoder: OpusDecoder,
    /// Interleaved float samples, oldest first.
    pcm: VecDeque<f32>,
    channels: usize,
    sample_rate: u32,
    /// Frame position the next byte of `pcm` represents.
    next_frame: u64,
    /// How much to hold before playback starts or resumes, in milliseconds.
    /// Moves between `floor_ms` and `ceiling_ms` as the link proves itself.
    target_ms: u32,
    floor_ms: u32,
    ceiling_ms: u32,
    /// When the target last changed, or last had a chance to decay.
    last_adjust: std::time::Instant,
    /// Whether enough has accumulated to start.
    playing: bool,

    pub underruns: u32,
    pub gaps: u32,
    pub late: u32,
    /// How often the queue had to be trimmed back to the target.
    pub trimmed: u32,
}

impl JitterBuffer {
    pub fn new(sample_rate: u32, channels: u16, priority: LatencyPriority) -> Result<Self> {
        let decoder = OpusDecoder::new(sample_rate, channels)
            .context("could not start the Opus decoder for playback")?;
        let floor_ms = priority.audio_buffer_floor_ms();
        Ok(Self {
            decoder,
            pcm: VecDeque::new(),
            channels: channels as usize,
            sample_rate,
            next_frame: 0,
            target_ms: floor_ms,
            floor_ms,
            ceiling_ms: priority.audio_buffer_ceiling_ms().max(floor_ms),
            last_adjust: std::time::Instant::now(),
            playing: false,
            underruns: 0,
            gaps: 0,
            late: 0,
            trimmed: 0,
        })
    }

    /// How much the buffer is currently trying to hold, in milliseconds.
    ///
    /// Shown in the stats row: on a link that is causing trouble this is the
    /// number that explains where the audio delay went.
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    /// How much audio is queued, in milliseconds.
    pub fn buffered_ms(&self) -> u32 {
        let frames = self.pcm.len() / self.channels.max(1);
        (frames as u64 * 1000 / self.sample_rate.max(1) as u64) as u32
    }

    /// Accept one block from the network.
    ///
    /// `first_frame` places the block on the timeline. A block that starts later
    /// than expected means packets were lost, and the difference is concealed
    /// rather than silently closed up — closing it up would shorten the
    /// timeline and put audio permanently ahead of video.
    pub fn push(&mut self, packets: &[u8], first_frame: u64, frames: u32) -> Result<()> {
        // First block seen sets the origin; there is nothing before it.
        if self.next_frame == 0 && self.pcm.is_empty() {
            self.next_frame = first_frame;
        }

        if first_frame > self.next_frame {
            let missing = (first_frame - self.next_frame) as usize;
            self.gaps += 1;
            self.conceal_frames(missing);
        } else if first_frame + frames as u64 <= self.next_frame {
            // Entirely in the past. Playing it would go backwards in time.
            self.late += 1;
            return Ok(());
        }

        let decoded = self.decoder.decode(packets)?;
        self.pcm.extend(decoded.iter().copied());
        self.next_frame += (decoded.len() / self.channels) as u64;

        if !self.playing && self.pcm.len() / self.channels >= self.frames_for(self.target_ms) {
            self.playing = true;
        }
        Ok(())
    }

    /// Fill `missing` frames with the best guess available.
    fn conceal_frames(&mut self, missing: usize) {
        let mut filled = 0;
        while filled < missing {
            let concealed = self.decoder.conceal();
            if concealed.is_empty() {
                break;
            }
            let frames = concealed.len() / self.channels;
            self.pcm.extend(concealed.iter().copied());
            filled += frames;
        }
        // Whatever concealment could not cover is silence, which is still
        // better than a hole: the timeline has to stay the right length.
        for _ in filled..missing {
            for _ in 0..self.channels {
                self.pcm.push_back(0.0);
            }
        }
        self.next_frame += missing as u64;
    }

    /// Take exactly `frames` frames for the device, whatever the state.
    ///
    /// Never returns short. A device given less than it asked for repeats its
    /// previous contents, which is far more audible than silence.
    pub fn pull(&mut self, frames: usize, out: &mut Vec<f32>) {
        out.clear();
        let wanted = frames * self.channels;

        if !self.playing {
            // Still filling. Silence now buys a buffer that will not underrun
            // on the next packet.
            out.resize(wanted, 0.0);
            return;
        }

        while out.len() < wanted {
            match self.pcm.pop_front() {
                Some(sample) => out.push(sample),
                None => break,
            }
        }
        if out.len() < wanted {
            self.underruns += 1;
            // Drop back to buffering so the next burst rebuilds a cushion,
            // rather than underrunning again on every callback.
            self.playing = false;
            out.resize(wanted, 0.0);
            self.grow();
        } else {
            self.decay();
        }

        // A buffer that has grown past the target is latency nobody asked for —
        // the network caught up after a stall, and without this it stays that
        // deep forever. Trimmed back to the target rather than to the ceiling,
        // so one correction fixes it instead of leaving it half wrong.
        let queued = self.pcm.len() / self.channels;
        let trim_above = self.frames_for(self.trim_above_ms());
        if queued > trim_above {
            let keep = self.frames_for(self.target_ms);
            let excess = (queued - keep) * self.channels;
            self.pcm.drain(..excess);
            self.trimmed += 1;
            debug!(
                "Audio: trimmed {} ms of backlog",
                (queued - keep) * 1000 / self.sample_rate.max(1) as usize
            );
        }
    }

    /// Where the queue is considered backlogged rather than merely cushioned.
    ///
    /// Twice the target, but never so tight that ordinary jitter around a small
    /// target reads as a backlog and gets trimmed — trimming what the buffer is
    /// deliberately holding would guarantee the next underrun.
    fn trim_above_ms(&self) -> u32 {
        (self.target_ms * 2).max(self.target_ms + 40)
    }

    /// The link broke this buffer. Ask for more.
    fn grow(&mut self) {
        let grown = (self.target_ms + GROW_ON_UNDERRUN_MS).min(self.ceiling_ms);
        if grown != self.target_ms {
            debug!("Audio: buffer {} ms -> {grown} ms after an underrun", self.target_ms);
            self.target_ms = grown;
        }
        // Reset either way: at the ceiling there is nothing left to give, and
        // decaying straight after an underrun would undo the growth that just
        // happened.
        self.last_adjust = std::time::Instant::now();
    }

    /// Nothing has gone wrong for a while. Give a little back.
    fn decay(&mut self) {
        if self.target_ms <= self.floor_ms {
            return;
        }
        if self.last_adjust.elapsed() < DECAY_INTERVAL {
            return;
        }
        self.target_ms = self.target_ms.saturating_sub(DECAY_STEP_MS).max(self.floor_ms);
        self.last_adjust = std::time::Instant::now();
    }

    /// Frames representing `ms` at this stream's rate.
    fn frames_for(&self, ms: u32) -> usize {
        (self.sample_rate as u64 * ms as u64 / 1000) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_codec::{OpusEncoder, DEFAULT_BITRATE, SAMPLE_RATE};

    fn encoder() -> Option<OpusEncoder> {
        OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE).ok()
    }

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// A link that keeps breaking the buffer gets a bigger one.
    ///
    /// The fixed 20 ms this replaced is why audio on a long-distance link
    /// stuttered continuously: every jitter spike past it underran, an underrun
    /// stops playback until the buffer refills, and the buffer refilled to the
    /// same 20 ms that had just failed.
    #[test]
    fn the_buffer_grows_when_the_link_keeps_starving_it() {
        let (Some(mut encoder), Ok(mut buffer)) =
            (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };
        let start = buffer.target_ms();
        let mut out = Vec::new();
        let mut frame = 0u64;

        // Three rounds of "enough arrives to start playing, then the device asks
        // for more than was ever sent". Filling first matters: a buffer that has
        // not started yet is not underrunning, it is still filling, and counting
        // that would grow the target for a link doing nothing wrong.
        for _ in 0..3 {
            for _ in 0..12 {
                let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
                if !blob.is_empty() {
                    buffer.push(&blob, frame, 480).expect("push");
                    frame += 480;
                }
            }
            buffer.pull(SAMPLE_RATE as usize / 4, &mut out);
        }
        assert!(buffer.underruns > 0, "the test did not manage to starve the buffer");

        assert!(
            buffer.target_ms() > start,
            "target stayed at {start} ms after {} underruns",
            buffer.underruns
        );
        assert!(
            buffer.target_ms() <= LatencyPriority::BALANCED.audio_buffer_ceiling_ms(),
            "the buffer must not grow without limit"
        );
    }

    /// And the trade is the one the slider is already about.
    #[test]
    fn the_latency_end_buffers_less_than_the_quality_end() {
        let latency = LatencyPriority::LATENCY;
        let quality = LatencyPriority::QUALITY;
        assert!(latency.audio_buffer_floor_ms() < quality.audio_buffer_floor_ms());
        assert!(latency.audio_buffer_ceiling_ms() < quality.audio_buffer_ceiling_ms());
        // And neither end may invert its own bounds.
        for priority in [latency, quality, LatencyPriority::BALANCED] {
            assert!(priority.audio_buffer_floor_ms() < priority.audio_buffer_ceiling_ms());
        }
    }

    /// The device must always get exactly what it asked for.
    ///
    /// This is the property everything else here serves: a short buffer makes
    /// the device repeat its previous contents, which is the loudest possible
    /// way to fail.
    #[test]
    fn a_pull_is_never_short_however_empty_the_buffer_is() {
        let Ok(mut buffer) = JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED) else { return };
        let mut out = Vec::new();

        // Nothing pushed at all.
        buffer.pull(480, &mut out);
        assert_eq!(out.len(), 480 * 2, "must still hand the device a full buffer");
        assert!(out.iter().all(|&s| s == 0.0), "and it must be silence, not stale memory");
    }

    #[test]
    fn playback_waits_for_a_cushion_before_it_starts() {
        let (Some(mut encoder), Ok(mut buffer)) = (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };

        // One packet is far less than the 40 ms target, so nothing plays yet.
        let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
        if !blob.is_empty() {
            buffer.push(&blob, 0, 480).expect("push");
        }
        let mut out = Vec::new();
        buffer.pull(480, &mut out);
        assert!(out.iter().all(|&s| s == 0.0), "must not start on a single packet");

        // Push past the target and it starts.
        let mut frame = 480u64;
        for _ in 0..12 {
            let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
            if !blob.is_empty() {
                buffer.push(&blob, frame, 480).expect("push");
                frame += 480;
            }
        }
        assert!(buffer.buffered_ms() >= buffer.target_ms(), "should have a cushion now");
        buffer.pull(480, &mut out);
        assert!(out.iter().any(|&s| s != 0.0), "should be playing real audio");
    }

    /// A gap in the frame positions must lengthen the timeline, not vanish.
    ///
    /// Silently closing a gap would leave audio permanently ahead of video by
    /// however much was lost — a sync error that accumulates and never recovers.
    #[test]
    fn lost_packets_are_filled_so_the_timeline_keeps_its_length() {
        let (Some(mut encoder), Ok(mut buffer)) = (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };

        let mut sent_frames = 0u64;
        for _ in 0..16 {
            let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
            if !blob.is_empty() {
                buffer.push(&blob, sent_frames, 480).expect("push");
                sent_frames += 480;
            }
        }
        let before = buffer.buffered_ms();

        // The next block arrives 5 packets later than it should have: 2400
        // frames went missing on the wire.
        let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
        if !blob.is_empty() {
            buffer.push(&blob, sent_frames + 2400, 480).expect("push");
        }

        assert_eq!(buffer.gaps, 1, "the gap must be noticed");
        let after = buffer.buffered_ms();
        let gap_ms = 2400 * 1000 / SAMPLE_RATE;
        assert!(
            after >= before + gap_ms,
            "buffer went from {before}ms to {after}ms — the {gap_ms}ms gap was swallowed"
        );
    }

    #[test]
    fn a_block_from_the_past_is_dropped_rather_than_played_backwards() {
        let (Some(mut encoder), Ok(mut buffer)) = (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };
        let mut frame = 0u64;
        for _ in 0..16 {
            let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
            if !blob.is_empty() {
                buffer.push(&blob, frame, 480).expect("push");
                frame += 480;
            }
        }
        let queued = buffer.buffered_ms();

        // A straggler for a position already played.
        let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
        if !blob.is_empty() {
            buffer.push(&blob, 0, 480).expect("push");
        }
        assert_eq!(buffer.late, 1, "the straggler must be recognised as late");
        assert_eq!(buffer.buffered_ms(), queued, "and must not be queued");
    }

    /// A stall followed by a flood must not become permanent latency.
    #[test]
    fn a_backlog_past_the_ceiling_is_shed_rather_than_played() {
        let (Some(mut encoder), Ok(mut buffer)) = (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };
        // Far more than MAX_LATENCY_MS worth: the network caught up all at once.
        let mut frame = 0u64;
        for _ in 0..120 {
            let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
            if !blob.is_empty() {
                buffer.push(&blob, frame, 480).expect("push");
                frame += 480;
            }
        }
        assert!(buffer.buffered_ms() > buffer.target_ms() * 3, "test needs an actual backlog");

        let mut out = Vec::new();
        buffer.pull(480, &mut out);
        assert!(
            buffer.buffered_ms() <= buffer.target_ms() + 20,
            "still holding {} ms after a pull — trimmed to the ceiling rather \
             than back to the target",
            buffer.buffered_ms()
        );
        assert_eq!(buffer.trimmed, 1, "the trim must be recorded");
    }

    /// Latency must not ratchet up and stay there.
    ///
    /// The bug this is here for: the buffer only shed above a 200 ms ceiling,
    /// so a queue that grew during one stall stayed that deep for the rest of
    /// the session. A real two-machine test measured 190-200 ms end to end.
    #[test]
    fn a_stall_does_not_leave_permanent_latency_behind() {
        let (Some(mut encoder), Ok(mut buffer)) = (encoder(), JitterBuffer::new(SAMPLE_RATE, 2, LatencyPriority::BALANCED))
        else {
            return;
        };
        let mut out = Vec::new();
        let mut frame = 0u64;

        // A burst arrives all at once, as it would after the network unblocks.
        for _ in 0..40 {
            let blob = encoder.encode(&tone(480)).expect("encode").to_vec();
            if !blob.is_empty() {
                buffer.push(&blob, frame, 480).expect("push");
                frame += 480;
            }
        }
        assert!(buffer.buffered_ms() > buffer.target_ms() * 3, "test needs an actual backlog");

        // Play it out at the device's rate for a while.
        for _ in 0..20 {
            buffer.pull(480, &mut out);
        }
        assert!(
            buffer.buffered_ms() <= buffer.target_ms() + 20,
            "settled at {} ms, which is latency that never goes away",
            buffer.buffered_ms()
        );
    }
}
