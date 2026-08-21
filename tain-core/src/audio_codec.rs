//! Opus encode and decode for the audio stream.
//!
//! ## Why Opus and why these settings
//!
//! See `tain-core/ffmpeg/db_audio.c` for the parameters and the reasoning. In
//! short: it is what every low-latency streamer uses, libopus is already inside
//! the avcodec linked for video decode, and the preset is tuned for music and
//! game audio on a link that drops packets rather than for speech on a clean
//! one.
//!
//! ## Why the encoder buffers
//!
//! Opus encodes exact frame sizes — 480 samples at 48 kHz for a 10 ms frame —
//! and WASAPI hands over whatever the audio engine had ready, which is neither
//! constant nor a multiple of anything. The shim accumulates and emits whole
//! packets, so a caller can push a capture buffer of any length and get back
//! however many packets that completed.
//!
//! ## Framing
//!
//! [`OpusEncoder::encode`] returns packets back to back, each behind a two-byte
//! big-endian length. Opus packets carry no length of their own, so something
//! has to say where each ends; doing it here means the transport moves one
//! opaque blob per capture buffer instead of negotiating packet boundaries.

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::Result;
use tracing::*;

#[repr(C)]
struct DbOpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
struct DbOpusDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn db_opus_encoder_new(
        sample_rate: i32,
        channels: i32,
        bitrate: i32,
        err: *mut u8,
        err_len: i32,
    ) -> *mut DbOpusEncoder;
    fn db_opus_frame_size(e: *mut DbOpusEncoder) -> i32;
    fn db_opus_encode(
        e: *mut DbOpusEncoder,
        samples: *const f32,
        frames: i32,
        out: *mut u8,
        out_cap: i32,
    ) -> i32;
    fn db_opus_encoder_free(e: *mut DbOpusEncoder);

    fn db_opus_decoder_new(
        sample_rate: i32,
        channels: i32,
        err: *mut u8,
        err_len: i32,
    ) -> *mut DbOpusDecoder;
    fn db_opus_decode(
        d: *mut DbOpusDecoder,
        data: *const u8,
        len: i32,
        out: *mut f32,
        out_cap_frames: i32,
    ) -> i32;
    fn db_opus_decoder_free(d: *mut DbOpusDecoder);
}

/// What the stream runs at. 48 kHz is Opus's native rate and what almost every
/// endpoint mixes at.
pub const SAMPLE_RATE: u32 = 48_000;

/// Bitrates offered to the user, in kbit/s.
///
/// 160 is the default: transparent for nearly all content, and there is
/// essentially nothing left to gain above 256.
pub const BITRATES_KBPS: [u32; 3] = [96, 160, 256];

/// The default, in bits per second.
pub const DEFAULT_BITRATE: u32 = 160_000;

fn message(err: &[u8]) -> String {
    let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
    String::from_utf8_lossy(&err[..end]).into_owned()
}

/// Encodes interleaved float32 into Opus packets.
pub struct OpusEncoder {
    handle: *mut DbOpusEncoder,
    channels: u16,
    packet_frames: usize,
    scratch: Vec<u8>,
}

// SAFETY: the handle is owned solely here and the type is driven from one
// thread — the audio pipeline's, never shared.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        // Before any call into the shim: a delay-load failure is a structured
        // exception, not an error this could return. See `ffmpeg_runtime`.
        super::ffmpeg_runtime::require()?;
        anyhow::ensure!(
            (1..=2).contains(&channels),
            "Opus needs 1 or 2 channels, got {channels}"
        );
        let mut err = [0u8; 256];
        // SAFETY: the error buffer is valid for its stated length; the handle is
        // checked before use.
        let handle = unsafe {
            db_opus_encoder_new(
                sample_rate as i32,
                channels as i32,
                bitrate as i32,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        anyhow::ensure!(!handle.is_null(), "could not start the Opus encoder: {}", message(&err));

        // SAFETY: the handle is non-null.
        let packet_frames = unsafe { db_opus_frame_size(handle) }.max(0) as usize;
        info!(
            "Audio: Opus at {} kbit/s, {sample_rate} Hz, {channels} channels, \
             {:.0} ms frames",
            bitrate / 1000,
            packet_frames as f64 * 1000.0 / sample_rate as f64,
        );
        Ok(Self { handle, channels, packet_frames, scratch: Vec::new() })
    }

    /// Frames in one Opus packet.
    pub fn packet_frames(&self) -> usize {
        self.packet_frames
    }

    /// Encode interleaved float32, returning length-prefixed packets.
    ///
    /// The result is empty when the input did not complete a frame, which is
    /// normal and not an error — the remainder is held for the next call.
    pub fn encode(&mut self, samples: &[f32]) -> Result<&[u8]> {
        let channels = self.channels as usize;
        anyhow::ensure!(
            samples.len().is_multiple_of(channels),
            "{} samples is not whole frames of {channels} channels",
            samples.len()
        );
        let frames = samples.len() / channels;

        // Opus never exceeds 1275 bytes per packet per channel; this is that
        // with room to spare, plus the two-byte length on each.
        let packets = frames / self.packet_frames.max(1) + 2;
        self.scratch.resize(packets * (1275 * channels + 2), 0);

        // SAFETY: both buffers are valid for the lengths passed, and the shim
        // writes no more than `out_cap`.
        let written = unsafe {
            db_opus_encode(
                self.handle,
                samples.as_ptr(),
                frames as i32,
                self.scratch.as_mut_ptr(),
                self.scratch.len() as i32,
            )
        };
        anyhow::ensure!(written >= 0, "Opus encode failed: error {written}");
        Ok(&self.scratch[..written as usize])
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: produced by `db_opus_encoder_new`, freed once.
        unsafe { db_opus_encoder_free(self.handle) };
    }
}

/// Decodes Opus packets back to interleaved float32.
pub struct OpusDecoder {
    handle: *mut DbOpusDecoder,
    channels: u16,
    pcm: Vec<f32>,
    /// The most recent decoded output, for concealing what comes after it.
    last: Vec<f32>,
    /// How many packets have been concealed since the last real one.
    concealed: u32,
}

// SAFETY: as for the encoder.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self> {
        super::ffmpeg_runtime::require()?;
        let mut err = [0u8; 256];
        // SAFETY: as for the encoder.
        let handle = unsafe {
            db_opus_decoder_new(
                sample_rate as i32,
                channels as i32,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        anyhow::ensure!(!handle.is_null(), "could not start the Opus decoder: {}", message(&err));
        Ok(Self { handle, channels, pcm: Vec::new(), last: Vec::new(), concealed: 0 })
    }

    /// Decode one length-prefixed blob as produced by [`OpusEncoder::encode`].
    pub fn decode(&mut self, blob: &[u8]) -> Result<&[f32]> {
        let channels = self.channels as usize;
        self.pcm.clear();

        let mut offset = 0;
        while offset + 2 <= blob.len() {
            let len = u16::from_be_bytes([blob[offset], blob[offset + 1]]) as usize;
            offset += 2;
            anyhow::ensure!(
                offset + len <= blob.len(),
                "packet claims {len} bytes but only {} remain",
                blob.len() - offset
            );
            let packet = &blob[offset..offset + len];
            offset += len;
            self.decode_one(packet, channels)?;
        }
        self.concealed = 0;
        self.last.clear();
        self.last.extend_from_slice(&self.pcm);
        Ok(&self.pcm)
    }

    /// Fill in for one lost packet.
    ///
    /// # Why this is not libavcodec's job
    ///
    /// libopus conceals loss natively, but libavcodec exposes no way to ask for
    /// it: passing a null packet to `avcodec_send_packet` means *end of
    /// stream*, which puts the decoder into draining mode and breaks every
    /// packet after it. The first version of this did exactly that.
    ///
    /// So concealment happens here instead, without touching decoder state: the
    /// previous output is repeated at falling gain. It is cruder than what
    /// libopus would do, and far better than the click a hole in the timeline
    /// produces. Successive losses fade to silence rather than looping audibly.
    pub fn conceal(&mut self) -> &[f32] {
        self.pcm.clear();
        if self.last.is_empty() {
            return &self.pcm;
        }
        self.concealed += 1;
        // Down roughly 6 dB per lost packet; four in a row is silence, which is
        // the right answer once there is nothing left worth extrapolating from.
        let gain = 0.5f32.powi(self.concealed as i32);
        if gain < 0.02 {
            return &self.pcm;
        }
        self.pcm.extend(self.last.iter().map(|s| s * gain));
        &self.pcm
    }

    fn decode_one(&mut self, packet: &[u8], channels: usize) -> Result<()> {
        // 120 ms is the longest an Opus packet can be, at 48 kHz.
        const MAX_FRAMES: usize = 5760;
        let base = self.pcm.len();
        self.pcm.resize(base + MAX_FRAMES * channels, 0.0);

        // SAFETY: the output has room for MAX_FRAMES frames from `base`, which
        // is what the shim is told; it writes no more.
        let frames = unsafe {
            db_opus_decode(
                self.handle,
                packet.as_ptr(),
                packet.len() as i32,
                self.pcm[base..].as_mut_ptr(),
                MAX_FRAMES as i32,
            )
        };
        if frames < 0 {
            self.pcm.truncate(base);
            anyhow::bail!("Opus decode failed: error {frames}");
        }
        self.pcm.truncate(base + frames as usize * channels);
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: produced by `db_opus_decoder_new`, freed once.
        unsafe { db_opus_decoder_free(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone, so a decode can be checked against something with structure.
    fn tone(frames: usize, channels: usize, hz: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let v = (t * hz * std::f32::consts::TAU).sin() * 0.5;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn audio_survives_a_round_trip_recognisably() {
        let Ok(mut encoder) = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE) else {
            eprintln!("no libopus in this build — skipping");
            return;
        };
        let mut decoder = OpusDecoder::new(SAMPLE_RATE, 2).expect("decoder");

        // Half a second of 440 Hz, pushed in awkward chunk sizes on purpose:
        // WASAPI does not hand over multiples of the Opus frame size, and the
        // encoder has to hold the remainder rather than dropping it.
        let mut sent = 0usize;
        let mut got = 0usize;
        let mut energy = 0.0f32;
        for chunk in 0..40 {
            let frames = 500 + (chunk % 7) * 31;
            let pcm = tone(frames, 2, 440.0);
            sent += frames;
            let blob = encoder.encode(&pcm).expect("encode").to_vec();
            if blob.is_empty() {
                continue;
            }
            let out = decoder.decode(&blob).expect("decode");
            got += out.len() / 2;
            energy += rms(out);
        }

        assert!(got > 0, "nothing decoded across {sent} frames");
        // Opus has a fixed algorithmic delay, so the output trails the input by
        // a packet or two; it must not trail by more than that.
        let missing = sent.saturating_sub(got);
        assert!(
            missing < sent / 4,
            "decoded {got} of {sent} frames — too much was lost, not just latency"
        );
        assert!(energy > 0.0, "decoded silence from a tone");
    }

    /// Compression has to actually happen, or there is no point to any of this.
    #[test]
    fn the_stream_is_far_smaller_than_the_samples_it_came_from() {
        let Ok(mut encoder) = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE) else {
            eprintln!("no libopus in this build — skipping");
            return;
        };
        let frames = SAMPLE_RATE as usize; // one second
        let pcm = tone(frames, 2, 440.0);
        let raw_bytes = pcm.len() * 4;

        let mut coded = 0usize;
        for chunk in pcm.chunks(4800) {
            coded += encoder.encode(chunk).expect("encode").len();
        }
        assert!(coded > 0, "encoder produced nothing");

        // 48 kHz stereo float is ~3 Mbit/s; 160 kbit/s Opus is about 5% of that.
        let ratio = coded as f64 / raw_bytes as f64;
        assert!(ratio < 0.15, "only compressed to {:.1}% of raw", ratio * 100.0);
        println!("{raw_bytes} bytes of PCM to {coded} bytes of Opus ({:.1}%)", ratio * 100.0);
    }

    /// A lost packet must produce sound, not a hole.
    #[test]
    fn concealment_fills_a_gap_rather_than_leaving_silence() {
        let Ok(mut encoder) = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE) else {
            eprintln!("no libopus in this build — skipping");
            return;
        };
        let mut decoder = OpusDecoder::new(SAMPLE_RATE, 2).expect("decoder");

        // Prime the decoder so it has a frame to extrapolate from.
        for _ in 0..10 {
            let blob = encoder.encode(&tone(480, 2, 440.0)).expect("encode").to_vec();
            if !blob.is_empty() {
                decoder.decode(&blob).expect("decode");
            }
        }

        let filled = decoder.conceal();
        assert!(!filled.is_empty(), "concealment produced no samples at all");
        let first = rms(filled);
        assert!(first > 0.0, "concealment produced silence where sound was expected");
        println!("concealed {} frames at rms {first:.4}", filled.len() / 2);

        // Successive losses fade rather than looping the same block audibly.
        let second = rms(decoder.conceal());
        assert!(second < first, "a second loss must be quieter, not a repeat");

        // And a real packet afterwards must still decode — the bug this
        // replaced put libavcodec into draining mode and broke everything after
        // the first concealed packet.
        let blob = encoder.encode(&tone(480, 2, 440.0)).expect("encode").to_vec();
        if !blob.is_empty() {
            let out = decoder.decode(&blob).expect("decoder must survive a concealed gap");
            assert!(!out.is_empty(), "no audio after concealment");
        }
    }

    #[test]
    fn samples_that_are_not_whole_frames_are_refused() {
        let Ok(mut encoder) = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE) else {
            return;
        };
        // Three samples cannot be whole stereo frames.
        assert!(encoder.encode(&[0.0, 0.0, 0.0]).is_err());
    }

    #[test]
    fn a_truncated_blob_is_an_error_rather_than_a_read_past_the_end() {
        let Ok(mut decoder) = OpusDecoder::new(SAMPLE_RATE, 2) else { return };
        // Claims 300 bytes, carries 2.
        assert!(decoder.decode(&[0x01, 0x2c, 0xff, 0xff]).is_err());
    }
}
