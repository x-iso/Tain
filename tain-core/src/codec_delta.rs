//! Lossless delta + zstd frame coding.
//!
//! A desktop is mostly static: between two frames, the overwhelming majority of
//! pixels are byte-identical. XOR-ing against the previous frame turns those
//! into runs of zero, which zstd collapses to almost nothing. A typing cursor on
//! an otherwise still screen costs a few kilobytes instead of several megabytes.
//!
//! It is **lossless**, needs no GPU, and adds no inter-frame latency beyond the
//! compression itself — which makes it the right default while hardware codecs
//! are unimplemented, and a genuinely good choice for text-heavy remote work
//! even once they exist.
//!
//! ## Frame types
//!
//! - **Keyframe**: the raw BGRA compressed on its own. Decodable standalone.
//! - **Delta**: `current XOR previous`, compressed. Requires the decoder to hold
//!   the previous frame.
//!
//! A keyframe is emitted on the first frame, whenever the resolution changes,
//! and every `keyframe_interval` frames so a guest that joins mid-stream or
//! drops a delta can resynchronise.

use anyhow::{Context, Result};

/// Compression level. 1 is the fastest; anything higher costs more latency than
/// it saves in bandwidth for interactive use.
const ZSTD_LEVEL: i32 = 1;

/// Encode BGRA frames as compressed deltas.
pub struct DeltaEncoder {
    previous: Option<Vec<u8>>,
    frames_since_keyframe: u32,
    keyframe_interval: u32,
}

impl DeltaEncoder {
    /// `keyframe_interval` of 0 means "only when structurally required".
    pub fn new(keyframe_interval: u32) -> Self {
        Self { previous: None, frames_since_keyframe: 0, keyframe_interval }
    }

    /// Compress one frame. Returns the payload and whether it is a keyframe.
    pub fn encode(&mut self, bgra: &[u8]) -> Result<(Vec<u8>, bool)> {
        let due = self.keyframe_interval != 0
            && self.frames_since_keyframe >= self.keyframe_interval;

        // A size change invalidates the reference frame entirely.
        let reusable = self
            .previous
            .as_ref()
            .filter(|prev| prev.len() == bgra.len() && !due);

        let (payload, is_keyframe) = match reusable {
            Some(prev) => {
                let mut delta = vec![0u8; bgra.len()];
                for (out, (cur, old)) in delta.iter_mut().zip(bgra.iter().zip(prev)) {
                    *out = cur ^ old;
                }
                (delta, false)
            }
            None => (bgra.to_vec(), true),
        };

        let compressed =
            zstd::encode_all(payload.as_slice(), ZSTD_LEVEL).context("zstd compression failed")?;

        self.previous = Some(bgra.to_vec());
        // The keyframe counts as the first frame of its group, so an interval
        // of N emits a keyframe every N frames rather than every N+1.
        self.frames_since_keyframe = if is_keyframe { 1 } else { self.frames_since_keyframe + 1 };

        Ok((compressed, is_keyframe))
    }

    /// Force the next frame to be a keyframe.
    ///
    /// Used when a guest joins, or asks to resynchronise after a decode error.
    pub fn request_keyframe(&mut self) {
        self.previous = None;
    }
}

/// Decode frames produced by [`DeltaEncoder`].
#[derive(Default)]
pub struct DeltaDecoder {
    previous: Option<Vec<u8>>,
}

impl DeltaDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decompress one frame back to BGRA.
    ///
    /// `expected_len` bounds the decompressed size so a malformed or hostile
    /// payload cannot make us allocate without limit.
    pub fn decode(
        &mut self,
        payload: &[u8],
        is_keyframe: bool,
        expected_len: usize,
    ) -> Result<Vec<u8>> {
        let raw = zstd::decode_all(payload).context("zstd decompression failed")?;
        if raw.len() != expected_len {
            anyhow::bail!(
                "frame decompressed to {} bytes, expected {expected_len}",
                raw.len()
            );
        }

        let frame = if is_keyframe {
            raw
        } else {
            // A delta is meaningless without the frame it was computed against.
            let previous = self
                .previous
                .as_ref()
                .filter(|p| p.len() == raw.len())
                .context("delta frame arrived with no matching reference frame")?;

            let mut out = vec![0u8; raw.len()];
            for (dst, (delta, old)) in out.iter_mut().zip(raw.iter().zip(previous)) {
                *dst = delta ^ old;
            }
            out
        };

        self.previous = Some(frame.clone());
        Ok(frame)
    }

    /// Drop the reference frame, e.g. after a session restart.
    pub fn reset(&mut self) {
        self.previous = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pseudo-random, so the keyframe does not compress to nothing on its own.
    /// A short repeating pattern would make every ratio in these tests trivially
    /// pass and prove nothing about delta coding.
    fn frame(width: usize, height: usize, seed: u8) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64 ^ ((seed as u64) << 32 | seed as u64);
        (0..width * height * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn first_frame_is_a_keyframe_and_round_trips() {
        let mut encoder = DeltaEncoder::new(0);
        let mut decoder = DeltaDecoder::new();
        let original = frame(16, 16, 0);

        let (payload, is_keyframe) = encoder.encode(&original).unwrap();
        assert!(is_keyframe, "nothing to delta against yet");

        let decoded = decoder.decode(&payload, is_keyframe, original.len()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn deltas_round_trip_across_a_sequence() {
        let mut encoder = DeltaEncoder::new(0);
        let mut decoder = DeltaDecoder::new();

        for seed in 0..8u8 {
            let original = frame(32, 24, seed);
            let (payload, is_keyframe) = encoder.encode(&original).unwrap();
            assert_eq!(is_keyframe, seed == 0, "only the first frame is a keyframe here");

            let decoded = decoder.decode(&payload, is_keyframe, original.len()).unwrap();
            assert_eq!(decoded, original, "frame {seed} did not round-trip");
        }
    }

    #[test]
    fn an_unchanged_screen_costs_almost_nothing() {
        // The whole point: a static desktop should collapse to a few bytes.
        let mut encoder = DeltaEncoder::new(0);
        let original = frame(320, 240, 7);

        let (keyframe, _) = encoder.encode(&original).unwrap();
        let (delta, is_keyframe) = encoder.encode(&original).unwrap();

        assert!(!is_keyframe);
        assert!(
            delta.len() < keyframe.len() / 50,
            "identical frame compressed to {} bytes vs {} for the keyframe",
            delta.len(),
            keyframe.len()
        );
        assert!(delta.len() < 200, "an all-zero delta should be tiny, got {}", delta.len());
    }

    #[test]
    fn a_small_change_costs_far_less_than_a_keyframe() {
        let mut encoder = DeltaEncoder::new(0);
        let original = frame(320, 240, 7);
        let (keyframe, _) = encoder.encode(&original).unwrap();

        // Change a handful of pixels, as a blinking cursor would.
        let mut edited = original.clone();
        for byte in edited.iter_mut().take(400) {
            *byte = byte.wrapping_add(9);
        }
        let (delta, _) = encoder.encode(&edited).unwrap();

        assert!(
            delta.len() < keyframe.len() / 4,
            "small edit cost {} bytes vs {} for a keyframe",
            delta.len(),
            keyframe.len()
        );
    }

    #[test]
    fn keyframe_interval_forces_periodic_standalone_frames() {
        let mut encoder = DeltaEncoder::new(3);
        let original = frame(8, 8, 1);

        let mut kinds = Vec::new();
        for _ in 0..8 {
            kinds.push(encoder.encode(&original).unwrap().1);
        }
        // First is a keyframe, then every third frame resynchronises.
        assert_eq!(kinds, vec![true, false, false, true, false, false, true, false]);
    }

    #[test]
    fn a_resolution_change_forces_a_keyframe() {
        let mut encoder = DeltaEncoder::new(0);
        encoder.encode(&frame(16, 16, 0)).unwrap();

        let (_, is_keyframe) = encoder.encode(&frame(32, 32, 0)).unwrap();
        assert!(is_keyframe, "a delta against a differently sized frame is meaningless");
    }

    #[test]
    fn requesting_a_keyframe_resynchronises_a_late_guest() {
        let mut encoder = DeltaEncoder::new(0);
        encoder.encode(&frame(16, 16, 0)).unwrap();
        assert!(!encoder.encode(&frame(16, 16, 1)).unwrap().1);

        encoder.request_keyframe();
        assert!(encoder.encode(&frame(16, 16, 2)).unwrap().1);
    }

    #[test]
    fn a_decoder_joining_mid_stream_rejects_a_delta() {
        // Without a reference frame a delta cannot be reconstructed, and
        // silently producing garbage would be far worse than erroring.
        let mut encoder = DeltaEncoder::new(0);
        let original = frame(16, 16, 0);
        encoder.encode(&original).unwrap();
        let (delta, is_keyframe) = encoder.encode(&frame(16, 16, 5)).unwrap();
        assert!(!is_keyframe);

        let mut fresh = DeltaDecoder::new();
        let err = fresh.decode(&delta, false, original.len()).unwrap_err();
        assert!(err.to_string().contains("reference frame"), "unhelpful error: {err}");
    }

    #[test]
    fn reset_discards_the_reference_frame() {
        let mut encoder = DeltaEncoder::new(0);
        let mut decoder = DeltaDecoder::new();
        let a = frame(16, 16, 0);
        let (k, kf) = encoder.encode(&a).unwrap();
        decoder.decode(&k, kf, a.len()).unwrap();

        decoder.reset();
        let (delta, _) = encoder.encode(&frame(16, 16, 3)).unwrap();
        assert!(decoder.decode(&delta, false, a.len()).is_err());
    }

    #[test]
    fn corrupt_payloads_are_rejected_rather_than_panicking() {
        let mut decoder = DeltaDecoder::new();
        assert!(decoder.decode(b"not zstd at all", true, 1024).is_err());
        assert!(decoder.decode(&[], true, 1024).is_err());
    }

    #[test]
    fn a_payload_of_the_wrong_size_is_rejected() {
        // Guards against a hostile or buggy peer inflating a tiny payload into
        // an enormous allocation, and against silent desync.
        let mut encoder = DeltaEncoder::new(0);
        let (payload, is_keyframe) = encoder.encode(&frame(16, 16, 0)).unwrap();

        let err = decoder_error(&payload, is_keyframe, 999_999);
        assert!(err.contains("expected 999999"), "unhelpful error: {err}");
    }

    fn decoder_error(payload: &[u8], is_keyframe: bool, expected: usize) -> String {
        DeltaDecoder::new().decode(payload, is_keyframe, expected).unwrap_err().to_string()
    }
}
