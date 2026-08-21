//! H.264 encoding and decoding via OpenH264.
//!
//! This is the codec that makes streaming a real desktop practical. The
//! lossless [`super::codec_delta`] path is pixel-exact and cheap on a static
//! screen, but the moment anything moves — a video, a scrolling page, a dragged
//! window — the XOR delta stops being mostly-zeroes and the bandwidth cost goes
//! through the roof. H.264 trades exactness for an order of magnitude less data.
//!
//! ## Why OpenH264 and not the GPU
//!
//! Software encoding works on every machine with no driver dependency, which is
//! what a "does it work at all" baseline needs. Hardware encoders (NVENC, Quick
//! Sync, AMF) are faster and cheaper on CPU, but each is a separate integration
//! with its own failure modes. [`EncoderBackend::Hardware`] is honest about not
//! being wired up yet rather than silently meaning the same thing as software.
//!
//! ## Patents
//!
//! OpenH264's *source* is BSD licensed, but H.264 itself is patent-encumbered.
//! Cisco's royalty coverage only applies to the binaries they distribute, not to
//! a copy built from source as this does. That is fine for personal and testing
//! use; shipping this commercially is a licensing question, not a code question.
//!
//! ## Dimensions
//!
//! The encoder is built lazily, on the first frame, because the sensible default
//! bitrate depends on the resolution — which capture, not configuration, decides.

use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, QpRange, RateControlMode, UsageType,
};
// YUVSource is what puts `dimensions()` on the decoder's output.
use openh264::formats::YUVSource;
use tracing::*;

use super::encoder::{EncodeConfig, RateControl};

/// Bits per pixel per frame assumed when no bitrate limit is set.
///
/// Screen content is far more compressible than camera video — large flat
/// regions, hard edges, and long stretches of no motion at all. 0.07 bpp keeps
/// text crisp at 1080p30 (~4 Mbit/s) while staying two orders of magnitude below
/// what the lossless path costs on the same content.
const DEFAULT_BITS_PER_PIXEL: u64 = 7; // hundredths, i.e. 0.07

/// Byte cap per H.264 slice, which is what determines how many cores encode a
/// frame — see the comment at the call site.
///
/// Derived from the frame so a small stream does not get carved into slices too
/// short to be worth the overhead, and a large one gets enough of them to fill
/// the cores. Clamped because both ends of that scale degrade.
fn slice_size_for(width: u16, height: u16) -> u32 {
    let pixels = width as u32 * height as u32;
    (pixels / 64).clamp(8_000, 60_000)
}

/// Bitrate to request when the user has not set a limit.
pub(crate) fn default_bitrate_bps(width: u16, height: u16, fps: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    let bps = pixels * fps.max(1) as u64 * DEFAULT_BITS_PER_PIXEL / 100;
    bps.clamp(500_000, 50_000_000) as u32
}

/// Map a 0-100 quality target onto an H.264 quantiser window as `(min, max)`.
///
/// QP runs the other way — 0 is lossless and 51 is unwatchable — so quality
/// climbing means QP falling. The window is kept narrow so constant-quality
/// actually holds a quality rather than drifting across the whole range.
///
/// Returns the pair rather than a `QpRange` because that type has no accessors,
/// which would leave the mapping untestable.
fn qp_window_for(quality: u8) -> (u8, u8) {
    let quality = quality.min(100) as i32;
    let centre = 45 - (quality * 33 / 100);
    (
        (centre - 6).clamp(0, 51) as u8,
        (centre + 6).clamp(0, 51) as u8,
    )
}

/// BGRA to I420, in the layout the encoder wants.
///
/// The crate's own `YUVBuffer::read_rgb` does this, and measured 26 ms on a
/// 2560x1600 frame — a quarter of the whole frame budget. Three things make it
/// slow, and none of them are inherent: it works in `f32`, it calls through a
/// closure per plane per pixel, and it iterates **column-major** over a
/// row-major image, so nearly every pixel read is a cache miss.
///
/// This does the same arithmetic in integers, in row order, two rows at a time
/// so the 2x2 chroma average falls out of the same pass. The coefficients are
/// the BT.601 limited-range set OpenH264 itself uses (66/129/25 for luma), so
/// colour is identical — matching mattered more than any cleverness here, since
/// a mismatch would show up as a subtle tint nobody could trace.
pub struct I420Frame {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Frame {
    pub fn new(width: usize, height: usize) -> Self {
        let (cw, ch) = (width / 2, height / 2);
        Self {
            y: vec![0; width * height],
            u: vec![0; cw * ch],
            v: vec![0; cw * ch],
            width,
            height,
        }
    }

    /// Overwrite from a BGRA frame of exactly this size.
    ///
    /// Reuses the plane allocations — at 1600p these are 6 MB, and reallocating
    /// them every frame would hand back much of what the conversion saves.
    pub fn read_bgra(&mut self, bgra: &[u8]) {
        debug_assert!(bgra.len() >= self.width * self.height * 4);
        let (w, h) = (self.width, self.height);
        let half_w = w / 2;

        for row_pair in 0..h / 2 {
            let y0 = row_pair * 2;
            let top = &bgra[y0 * w * 4..(y0 + 1) * w * 4];
            let bottom = &bgra[(y0 + 1) * w * 4..(y0 + 2) * w * 4];
            let (y_top, y_bottom) = {
                let (a, b) = self.y.split_at_mut((y0 + 1) * w);
                (&mut a[y0 * w..], &mut b[..w])
            };
            let chroma_row = row_pair * half_w;

            for col_pair in 0..half_w {
                let x0 = col_pair * 4 * 2;
                // B, G, R for all four pixels of the 2x2 block.
                let px = [
                    (top[x0] as i32, top[x0 + 1] as i32, top[x0 + 2] as i32),
                    (top[x0 + 4] as i32, top[x0 + 5] as i32, top[x0 + 6] as i32),
                    (bottom[x0] as i32, bottom[x0 + 1] as i32, bottom[x0 + 2] as i32),
                    (bottom[x0 + 4] as i32, bottom[x0 + 5] as i32, bottom[x0 + 6] as i32),
                ];

                let luma = |(b, g, r): (i32, i32, i32)| {
                    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
                };
                y_top[col_pair * 2] = luma(px[0]);
                y_top[col_pair * 2 + 1] = luma(px[1]);
                y_bottom[col_pair * 2] = luma(px[2]);
                y_bottom[col_pair * 2 + 1] = luma(px[3]);

                // Chroma is subsampled from the average of the block.
                let (b, g, r) = (
                    (px[0].0 + px[1].0 + px[2].0 + px[3].0) / 4,
                    (px[0].1 + px[1].1 + px[2].1 + px[3].1) / 4,
                    (px[0].2 + px[1].2 + px[2].2 + px[3].2) / 4,
                );
                self.u[chroma_row + col_pair] =
                    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                self.v[chroma_row + col_pair] =
                    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }

    /// Overwrite from an NV12 frame of exactly this size.
    ///
    /// NV12 and I420 hold the same samples in the same colour space; the only
    /// difference is that NV12 interleaves Cb and Cr in one plane where I420
    /// keeps them apart. So this is a copy and a de-interleave, with no
    /// arithmetic and no colour conversion — nothing here can shift the
    /// picture, which is the property that matters given how easily a stray
    /// coefficient becomes a tint nobody can trace.
    ///
    /// See [`H264Encoder::encode_nv12`] for why the software encoder has to
    /// accept this format.
    pub fn read_nv12(&mut self, nv12: &[u8]) {
        let (w, h) = (self.width, self.height);
        debug_assert!(nv12.len() >= w * h * 3 / 2);

        self.y.copy_from_slice(&nv12[..w * h]);

        let (half_w, half_h) = (w / 2, h / 2);
        let chroma = &nv12[w * h..];
        for row in 0..half_h {
            let src = &chroma[row * w..row * w + half_w * 2];
            let dst = row * half_w;
            for col in 0..half_w {
                self.u[dst + col] = src[col * 2];
                self.v[dst + col] = src[col * 2 + 1];
            }
        }
    }
}

impl YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Translate our rate-control choice into OpenH264's.
fn rate_control_for(mode: RateControl) -> RateControlMode {
    match mode {
        // A steady bitrate is exactly what RC_BITRATE_MODE targets.
        RateControl::Constant => RateControlMode::Bitrate,
        // Buffer-based lets quality float with complexity while still honouring
        // the ceiling — the closest thing OpenH264 has to VBR.
        RateControl::Variable => RateControlMode::Bufferbased,
        // Quality mode ignores the bitrate target and holds the QP instead.
        RateControl::ConstantQuality => RateControlMode::Quality,
    }
}

/// H.264 encoder wrapping OpenH264, producing an Annex-B stream per frame.
pub struct H264Encoder {
    config: EncodeConfig,
    /// Built on the first frame and rebuilt if the capture size changes.
    inner: Option<Encoder>,
    yuv: Option<I420Frame>,
    dimensions: (u16, u16),
    /// Frames since the last keyframe, for the caller-configured interval.
    since_keyframe: u32,
    /// Set when a guest has asked for one, cleared as soon as it is honoured.
    forced: bool,
    scratch: Vec<u8>,
}

impl H264Encoder {
    pub fn new(config: &EncodeConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            inner: None,
            yuv: None,
            dimensions: (0, 0),
            since_keyframe: 0,
            forced: false,
            scratch: Vec::new(),
        })
    }

    /// Adopt new encode settings, taking effect on the next frame.
    ///
    /// Dropping the encoder rather than poking at it: OpenH264 exposes runtime
    /// bitrate changes, but rebuilding is one code path instead of two and the
    /// adaptive controller changes settings at most once a second or so.
    pub fn reconfigure(&mut self, config: &EncodeConfig) {
        if self.config == *config {
            return;
        }
        self.config = config.clone();
        self.inner = None;
    }

    /// Build (or rebuild) the OpenH264 encoder for a given frame size.
    fn build(&mut self, width: u16, height: u16) -> Result<()> {
        let bitrate = match self.config.target_bitrate_kbps {
            0 => default_bitrate_bps(width, height, self.config.fps),
            kbps => kbps.saturating_mul(1000),
        };

        let cfg = EncoderConfig::new()
            // The difference between this and CameraVideoRealTime is large on
            // desktop content: it changes motion search and skip decisions to
            // suit sharp edges and static regions.
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(rate_control_for(self.config.rate_control))
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(self.config.fps.max(1) as f32))
            .qp({
                let (min, max) = qp_window_for(self.config.quality);
                QpRange::new(min, max)
            })
            // Dropping frames to hit a bitrate is better than falling behind
            // real time and streaming the past.
            .skip_frames(true)
            // This is what makes the encoder use more than one core, and it is
            // not obvious: OpenH264 parallelises *across slices*, and with no
            // slice configuration it sets SM_SINGLE_SLICE — one slice, therefore
            // one thread, no matter what `num_threads` says. Measured on a
            // 16-core CPU at 2560x1600: 74 ms single-slice against 24 ms here,
            // with `num_threads` alone making no difference whatsoever.
            //
            // The size is a byte cap per slice, so it scales with the frame:
            // too large and there are not enough slices to fill the cores, too
            // small and the per-slice overhead and coding loss start to show.
            // Measurements were flat from 8 KB to 60 KB, so this sits in the
            // middle rather than at an edge.
            .max_slice_len(slice_size_for(width, height))
            // Both are on by default and both are unsupported in screen-content
            // mode. Leaving them set makes OpenH264 print a warning per session
            // and then disable them anyway, so turn them off here instead.
            .adaptive_quantization(false)
            .background_detection(false)
            // Auto. Capped by the slice count above, which is the real limit.
            .num_threads(0);

        info!(
            "H.264 encoder: {}x{} @ {}fps, {} kbit/s, {:?}",
            width,
            height,
            self.config.fps,
            bitrate / 1000,
            self.config.rate_control
        );

        self.inner = Some(Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            cfg,
        )
        .map_err(|e| anyhow::anyhow!("could not start the H.264 encoder: {e}"))?);
        self.yuv = Some(I420Frame::new(width as usize, height as usize));
        self.dimensions = (width, height);
        self.since_keyframe = 0;
        Ok(())
    }

    /// Encode one BGRA frame. Returns the Annex-B payload and whether it is a keyframe.
    /// Make the next frame an IDR, whatever the interval says.
    pub fn request_keyframe(&mut self) {
        self.forced = true;
    }

    pub fn encode(&mut self, bgra: &[u8], width: u16, height: u16) -> Result<(Vec<u8>, bool)> {
        let expected = width as usize * height as usize * 4;
        if bgra.len() < expected {
            anyhow::bail!(
                "BGRA buffer too small for {width}x{height}: got {} bytes, need {expected}",
                bgra.len()
            );
        }
        self.encode_planes(width, height, |yuv| yuv.read_bgra(&bgra[..expected]))
    }

    /// Encode one NV12 frame, as the capture path produces when the GPU
    /// converts.
    ///
    /// # Why the software encoder needs this at all
    ///
    /// The pixel format is negotiated once, from the encoder that exists when
    /// the session starts, and a hardware encoder asks for NV12 so the capture
    /// side can convert on the GPU. If that hardware encoder then fails —
    /// wedged driver, exhausted encode sessions, a size it will not take — the
    /// fallback to the CPU is supposed to keep the session alive.
    ///
    /// It did the opposite. The software path spoke BGRA only, so the very next
    /// frame failed with "the CPU fallback cannot encode Nv12" and the session
    /// ended. A comment claimed the capture side refused to let that happen; it
    /// happened three times in one log, and it turned a recoverable driver
    /// hiccup into a host that could not be connected to at all until it was
    /// restarted.
    ///
    /// De-interleaving NV12's chroma into I420 is close to free next to the
    /// encode itself, which makes keeping the session the obvious trade.
    pub fn encode_nv12(&mut self, nv12: &[u8], width: u16, height: u16) -> Result<(Vec<u8>, bool)> {
        let expected = width as usize * height as usize * 3 / 2;
        if nv12.len() < expected {
            anyhow::bail!(
                "NV12 buffer too small for {width}x{height}: got {} bytes, need {expected}",
                nv12.len()
            );
        }
        self.encode_planes(width, height, |yuv| yuv.read_nv12(&nv12[..expected]))
    }

    /// The shared body of both entry points: everything except how the planes
    /// get filled.
    fn encode_planes(
        &mut self,
        width: u16,
        height: u16,
        fill: impl FnOnce(&mut I420Frame),
    ) -> Result<(Vec<u8>, bool)> {
        // H.264 needs even dimensions for 4:2:0 chroma. Capture already rounds
        // down to even, so this is a guard against a backend that doesn't.
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            anyhow::bail!("H.264 needs even dimensions, got {width}x{height}");
        }

        if self.inner.is_none() || self.dimensions != (width, height) {
            self.build(width, height)?;
        }

        // Ask for an IDR when the interval is up. Without one, a guest that
        // loses a packet has nothing to resynchronise against.
        let interval = self.config.keyframe_interval;
        let want_keyframe =
            std::mem::take(&mut self.forced) || (interval > 0 && self.since_keyframe >= interval);

        let yuv = self.yuv.as_mut().expect("built above");
        fill(yuv);

        let encoder = self.inner.as_mut().expect("built above");
        if want_keyframe {
            encoder.force_intra_frame();
        }

        let bitstream = encoder
            .encode(yuv)
            .map_err(|e| anyhow::anyhow!("H.264 encode failed: {e}"))?;

        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        self.scratch.clear();
        bitstream.write_vec(&mut self.scratch);

        self.since_keyframe = if is_keyframe { 0 } else { self.since_keyframe + 1 };

        // A skipped frame produces no NAL units at all. Passing an empty payload
        // to the guest would look like a decode failure, so report it as such
        // and let the caller drop it.
        if self.scratch.is_empty() {
            trace!("H.264 encoder skipped a frame to hold the bitrate");
        }

        Ok((std::mem::take(&mut self.scratch), is_keyframe))
    }
}

/// Decoded picture, already in the RGBA layout the renderer uploads.
pub struct DecodedPicture {
    pub width: usize,
    pub height: usize,
    /// RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// H.264 decoder wrapping OpenH264.
pub struct H264Decoder {
    inner: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let inner = Decoder::new()
            .map_err(|e| anyhow::anyhow!("could not start the H.264 decoder: {e}"))?;
        Ok(Self { inner })
    }

    /// Decode one Annex-B packet.
    ///
    /// `Ok(None)` means the packet was consumed but produced no picture yet —
    /// normal for parameter sets, and not an error.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        if packet.is_empty() {
            return Ok(None);
        }

        let decoded = self
            .inner
            .decode(packet)
            .map_err(|e| anyhow::anyhow!("H.264 decode failed: {e}"))
            .context("dropping frame")?;

        let Some(yuv) = decoded else { return Ok(None) };

        // Size the target from what the decoder actually produced. `write_rgba8`
        // panics on a mismatch, and the frame header is the peer's claim about
        // the picture, not the decoder's.
        let (width, height) = yuv.dimensions();
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);

        Ok(Some(DecodedPicture { width, height, rgba }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::VideoCodec;

    fn config() -> EncodeConfig {
        EncodeConfig {
            codec: VideoCodec::H264,
            keyframe_interval: 30,
            fps: 30,
            ..EncodeConfig::default()
        }
    }

    /// Pack an [`I420Frame`]'s planes back into NV12 byte order.
    fn to_nv12(frame: &I420Frame) -> Vec<u8> {
        let (w, h) = (frame.width, frame.height);
        let mut out = frame.y.clone();
        for i in 0..(w / 2) * (h / 2) {
            out.push(frame.u[i]);
            out.push(frame.v[i]);
        }
        out
    }

    /// NV12 in, the same samples out — no arithmetic, no colour shift.
    ///
    /// # The bug this is here for
    ///
    /// The software encoder spoke BGRA only, so when a hardware encoder failed
    /// mid-session the fallback refused the NV12 the capture path was already
    /// producing. The session died on the next frame, and every session after
    /// it died the same way until the app was restarted.
    ///
    /// This pins the conversion rather than the plumbing: NV12 and I420 hold
    /// identical samples, so anything other than an exact match means the
    /// de-interleave is wrong and the picture would come out with its chroma
    /// swapped or smeared.
    #[test]
    fn nv12_round_trips_through_the_software_encoder_unchanged() {
        let source = pattern(64, 32, 0);
        let mut from_bgra = I420Frame::new(64, 32);
        from_bgra.read_bgra(&source);

        let mut from_nv12 = I420Frame::new(64, 32);
        from_nv12.read_nv12(&to_nv12(&from_bgra));

        assert_eq!(from_nv12.y, from_bgra.y, "luma must be copied verbatim");
        assert_eq!(from_nv12.u, from_bgra.u, "Cb must come from the even bytes");
        assert_eq!(from_nv12.v, from_bgra.v, "Cr must come from the odd bytes");
    }

    /// The fallback has to actually encode, not merely accept the buffer.
    #[test]
    fn the_software_encoder_accepts_nv12_frames() {
        let mut encoder = H264Encoder::new(&config()).unwrap();
        let mut planes = I420Frame::new(64, 64);
        planes.read_bgra(&pattern(64, 64, 0));

        let (packet, is_keyframe) = encoder.encode_nv12(&to_nv12(&planes), 64, 64).unwrap();
        assert!(is_keyframe, "the first frame must stand alone");
        assert!(!packet.is_empty());

        // A buffer one byte short must be refused rather than read past its end.
        let short = vec![0u8; 64 * 64 * 3 / 2 - 1];
        assert!(encoder.encode_nv12(&short, 64, 64).is_err());
    }

    /// A recognisable pattern: flat colour blocks that survive lossy encoding.
    fn pattern(width: u16, height: u16, shift: u8) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                let left = x < width as usize / 2;
                // B, G, R, A
                data[i] = if left { 30 } else { 200u8.wrapping_add(shift) };
                data[i + 1] = if left { 30 } else { 60 };
                data[i + 2] = if left { 220u8.wrapping_add(shift) } else { 40 };
                data[i + 3] = 255;
            }
        }
        data
    }

    #[test]
    fn a_frame_survives_the_round_trip_at_the_right_size() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let mut dec = H264Decoder::new().unwrap();

        let src = pattern(64, 64, 0);
        let (packet, is_keyframe) = enc.encode(&src, 64, 64).unwrap();
        assert!(is_keyframe, "the first frame must stand alone");
        assert!(!packet.is_empty());

        let picture = dec.decode(&packet).unwrap().expect("first frame should decode");
        assert_eq!((picture.width, picture.height), (64, 64));
        assert_eq!(picture.rgba.len(), 64 * 64 * 4);
    }

    #[test]
    fn decoded_colour_resembles_the_source() {
        // Lossy, so this checks the picture is recognisably right — a channel
        // swap or a YUV mix-up would fail this by a mile.
        let mut enc = H264Encoder::new(&config()).unwrap();
        let mut dec = H264Decoder::new().unwrap();

        let src = pattern(64, 64, 0);
        let (packet, _) = enc.encode(&src, 64, 64).unwrap();
        let picture = dec.decode(&packet).unwrap().unwrap();

        // Sample well inside each block, away from the edge the codec smooths.
        let at = |x: usize, y: usize| {
            let i = (y * picture.width + x) * 4;
            (picture.rgba[i], picture.rgba[i + 1], picture.rgba[i + 2])
        };
        let (lr, _lg, lb) = at(16, 32);
        let (rr, _rg, rb) = at(48, 32);

        assert!(lr > 150, "left half should stay red-dominant, got r={lr}");
        assert!(lb < 90, "left half should stay red-dominant, got b={lb}");
        assert!(rb > 150, "right half should stay blue-dominant, got b={rb}");
        assert!(rr < 90, "right half should stay blue-dominant, got r={rr}");
    }

    #[test]
    fn h264_is_dramatically_smaller_than_the_raw_frame() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let src = pattern(320, 240, 0);
        let (packet, _) = enc.encode(&src, 320, 240).unwrap();

        let raw = 320 * 240 * 4;
        assert!(
            packet.len() < raw / 20,
            "keyframe was {} bytes against {raw} raw — compression is not working",
            packet.len()
        );
    }

    #[test]
    fn a_static_screen_costs_almost_nothing_after_the_keyframe() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let src = pattern(320, 240, 0);

        let (keyframe, _) = enc.encode(&src, 320, 240).unwrap();
        let (delta, is_keyframe) = enc.encode(&src, 320, 240).unwrap();

        assert!(!is_keyframe, "an unchanged frame should not need an IDR");
        assert!(
            delta.len() < keyframe.len() / 4,
            "unchanged frame cost {} bytes vs {} for the keyframe",
            delta.len(),
            keyframe.len()
        );
    }

    #[test]
    fn keyframes_come_back_on_the_configured_interval() {
        let cfg = EncodeConfig { keyframe_interval: 3, ..config() };
        let mut enc = H264Encoder::new(&cfg).unwrap();

        let mut keyframes = 0;
        for i in 0..12u8 {
            let (_, is_keyframe) = enc.encode(&pattern(64, 64, i * 5), 64, 64).unwrap();
            if is_keyframe {
                keyframes += 1;
            }
        }
        // The first frame plus roughly one every three after it. Scene-change
        // detection can add more, so this is a floor, not an equality.
        assert!(keyframes >= 3, "expected periodic keyframes, saw {keyframes}");
    }

    #[test]
    fn a_resolution_change_rebuilds_rather_than_corrupting() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let mut dec = H264Decoder::new().unwrap();

        let (a, _) = enc.encode(&pattern(64, 64, 0), 64, 64).unwrap();
        dec.decode(&a).unwrap();

        let (b, is_keyframe) = enc.encode(&pattern(96, 48, 0), 96, 48).unwrap();
        assert!(is_keyframe, "a new resolution has to start with a keyframe");

        let picture = dec.decode(&b).unwrap().expect("resized frame should decode");
        assert_eq!((picture.width, picture.height), (96, 48));
    }

    #[test]
    fn odd_dimensions_are_rejected_rather_than_producing_garbage() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let err = enc.encode(&pattern(64, 64, 0), 63, 64).unwrap_err();
        assert!(err.to_string().contains("even"));
    }

    #[test]
    fn an_undersized_buffer_is_rejected_rather_than_panicking() {
        let mut enc = H264Encoder::new(&config()).unwrap();
        let err = enc.encode(&pattern(8, 8, 0), 64, 64).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn garbage_never_panics_the_decoder() {
        let mut dec = H264Decoder::new().unwrap();
        // Whether OpenH264 reports an error or just produces nothing, neither
        // may take the guest session down.
        let _ = dec.decode(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(dec.decode(&[]).unwrap().is_none());
    }

    #[test]
    fn slices_scale_with_the_frame_and_stay_inside_the_useful_range() {
        // Too few slices and the encoder cannot use the cores; too many and the
        // per-slice overhead and coding loss dominate.
        let small = slice_size_for(640, 360);
        let large = slice_size_for(3840, 2160);
        assert!(small <= large);
        for (w, h) in [(64u16, 64u16), (1280, 720), (2560, 1600), (3840, 2160)] {
            let size = slice_size_for(w, h);
            assert!((8_000..=60_000).contains(&size), "{w}x{h} gave {size}");
        }
    }

    #[test]
    fn the_converter_agrees_with_the_one_it_replaced() {
        // The whole risk of hand-rolling this is a subtle colour shift nobody
        // can trace, so pin it against the crate's own implementation. Integer
        // versus f32 rounding allows a little slack, not a hue change.
        use openh264::formats::{BgraSliceU8, YUVBuffer};

        let (w, h) = (64usize, 32usize);
        let src = pattern(w as u16, h as u16, 0);

        let mut theirs = YUVBuffer::new(w, h);
        theirs.read_rgb(BgraSliceU8::new(&src, (w, h)));

        let mut ours = I420Frame::new(w, h);
        ours.read_bgra(&src);

        assert_eq!(ours.y().len(), theirs.y().len());
        assert_eq!(ours.strides(), theirs.strides());

        let worst = |a: &[u8], b: &[u8]| {
            a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u32).max().unwrap_or(0)
        };
        assert!(worst(ours.y(), theirs.y()) <= 2, "luma drifted");
        assert!(worst(ours.u(), theirs.u()) <= 2, "blue chroma drifted");
        assert!(worst(ours.v(), theirs.v()) <= 2, "red chroma drifted");
    }

    #[test]
    fn the_converter_places_luma_row_major() {
        // A transposed write would still round-trip through the encoder and
        // just look wrong, so check the layout directly: a frame whose left
        // half is dark must produce a dark *left* half of every luma row.
        let (w, h) = (16usize, 8usize);
        let src = pattern(w as u16, h as u16, 0);
        let mut frame = I420Frame::new(w, h);
        frame.read_bgra(&src);

        for row in 0..h {
            let line = &frame.y()[row * w..(row + 1) * w];
            let (left, right) = line.split_at(w / 2);
            let avg = |s: &[u8]| s.iter().map(|v| *v as u32).sum::<u32>() / s.len() as u32;
            assert_ne!(avg(left), avg(right), "row {row} lost its structure");
        }
    }

    #[test]
    fn the_converter_handles_the_smallest_legal_frame() {
        let mut frame = I420Frame::new(2, 2);
        frame.read_bgra(&[255u8; 2 * 2 * 4]);
        assert_eq!(frame.y().len(), 4);
        assert_eq!(frame.u().len(), 1);
        assert_eq!(frame.v().len(), 1);
        // White in, high luma out.
        assert!(frame.y().iter().all(|v| *v > 200));
    }

    #[test]
    fn an_unset_bitrate_scales_with_resolution_and_frame_rate() {
        let low = default_bitrate_bps(1280, 720, 30);
        let high = default_bitrate_bps(1920, 1080, 30);
        let faster = default_bitrate_bps(1920, 1080, 60);

        assert!(high > low, "a bigger screen should get more bits");
        assert!(faster > high, "more frames should get more bits");
        // Sanity: 1080p30 should land in the low single-digit megabits.
        assert!((2_000_000..8_000_000).contains(&high), "1080p30 asked for {high} bps");
    }

    #[test]
    fn the_bitrate_floor_and_ceiling_hold() {
        assert_eq!(default_bitrate_bps(2, 2, 1), 500_000, "tiny frames still get a usable floor");
        assert_eq!(
            default_bitrate_bps(u16::MAX, u16::MAX, 240),
            50_000_000,
            "an absurd request is capped"
        );
    }

    #[test]
    fn higher_quality_asks_for_a_lower_quantiser() {
        // QP is inverted: better quality means a smaller number.
        let (best_min, _) = qp_window_for(100);
        let (worst_min, _) = qp_window_for(0);
        assert!(best_min < worst_min, "quality 100 should quantise less than quality 0");

        // QpRange::new panics outside 0..=51 or with min > max, so every input
        // the settings UI can produce has to land inside that.
        for quality in [0u8, 25, 50, 75, 100, 255] {
            let (min, max) = qp_window_for(quality);
            assert!(min <= max);
            assert!(max <= 51);
        }
    }
}
