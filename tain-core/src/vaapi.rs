//! GPU video on Linux: VA-API decode and encode, through libtain-media.so.
//!
//! The Linux counterpart of `decode_ffmpeg` and `codec_mf`, over the shim in
//! `tain-core/ffmpeg/db_vaapi.c`. Intel and AMD GPUs decode and encode H.264
//! and H.265 this way, and AV1 on the newer ones — Intel Arc and Meteor Lake
//! onwards, AMD RDNA2 onwards for decode and RDNA3 for encode.
//!
//! ## Decode
//!
//! The picture is decoded on the GPU, copied back as NV12 and converted to
//! RGBA on the processor (`yuv::nv12_planes_to_rgba`), because that is what
//! the renderer takes. Drawing the surface without the copy — exporting it as
//! a DMA-BUF into the GL context — is the next step, and the Windows build's
//! equivalent is `gl_interop`.
//!
//! ## Encode
//!
//! Frames go up as NV12, from `yuv::bgra_to_nv12` when the capture side hands
//! over BGRA. FFmpeg's VA-API encoders take their rate control when they are
//! built, so a bitrate change rebuilds the encoder — one keyframe — where
//! Media Foundation changes it live. Changes under a fifth are left alone, so
//! the controller's small corrections do not cost a keyframe each.
//!
//! ## What the GPU can do
//!
//! [`can_decode`] and [`can_encode`] ask the driver's profile and entrypoint
//! lists, once per codec, so the settings window can label a codec this GPU
//! cannot encode the way Windows does.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::OnceLock;
use tracing::*;

use crate::codec_h264::{default_bitrate_bps, DecodedPicture, H264Encoder};
use crate::encoder::{EncodeConfig, EncodedFrame, EncoderBackend, VideoCodec, VideoEncoder};
use crate::screen_capture::PixelFormat;

#[repr(C)]
struct DbVaDecoder {
    _private: [u8; 0],
}

#[repr(C)]
struct DbVaEncoder {
    _private: [u8; 0],
}

// Must match the prototypes in db_vaapi.c.
super::ffmpeg_runtime::media_functions! {
    fn db_va_decoder_new(codec: i32, err: *mut u8, err_len: i32) -> *mut DbVaDecoder;
    fn db_va_decoder_name(d: *mut DbVaDecoder) -> *const std::ffi::c_char;
    fn db_va_decoder_send(d: *mut DbVaDecoder, data: *const u8, len: i32) -> i32;
    fn db_va_decoder_receive(
        d: *mut DbVaDecoder,
        y: *mut *const u8,
        y_stride: *mut i32,
        uv: *mut *const u8,
        uv_stride: *mut i32,
        width: *mut i32,
        height: *mut i32,
    ) -> i32;
    fn db_va_decoder_free(d: *mut DbVaDecoder);
    fn db_va_encoder_new(
        codec: i32,
        width: i32,
        height: i32,
        fps: i32,
        bitrate_kbps: i32,
        gop: i32,
        err: *mut u8,
        err_len: i32,
    ) -> *mut DbVaEncoder;
    fn db_va_encoder_name(e: *mut DbVaEncoder) -> *const std::ffi::c_char;
    fn db_va_encoder_encode(
        e: *mut DbVaEncoder,
        nv12: *const u8,
        force_keyframe: i32,
        out: *mut *const u8,
        out_len: *mut i32,
        is_keyframe: *mut i32,
    ) -> i32;
    fn db_va_encoder_free(e: *mut DbVaEncoder);
    fn db_va_supports(codec: i32, encode: i32, driver: *mut u8, driver_len: i32) -> i32;
}

/// The shim's codec numbering, which db_decode.c shares.
fn codec_index(codec: VideoCodec) -> Option<i32> {
    match codec {
        VideoCodec::H264 => Some(0),
        VideoCodec::H265 => Some(1),
        VideoCodec::Av1 => Some(2),
        _ => None,
    }
}

fn message(err: &[u8]) -> String {
    let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
    String::from_utf8_lossy(&err[..end]).into_owned()
}

fn c_string(text: *const std::ffi::c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the shim returns NUL-terminated strings it owns, valid for the
    // life of the object they describe; this copies before returning.
    unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
}

// ── What this GPU can do ─────────────────────────────────────────────────

/// One answer per codec and direction, worked out the first time it is asked.
/// The settings window asks every frame it is open; a GPU does not change.
static SUPPORT: [[OnceLock<bool>; 2]; 3] = [
    [OnceLock::new(), OnceLock::new()],
    [OnceLock::new(), OnceLock::new()],
    [OnceLock::new(), OnceLock::new()],
];
static DRIVER: OnceLock<Option<String>> = OnceLock::new();

fn supports(codec: VideoCodec, encode: bool) -> bool {
    let Some(index) = codec_index(codec) else { return false };
    *SUPPORT[index as usize][encode as usize].get_or_init(|| {
        if !super::ffmpeg_runtime::vaapi() {
            return false;
        }
        let mut driver = [0u8; 192];
        // SAFETY: vaapi() said the library and libva are loaded; the buffer is
        // valid for its stated length.
        let answer = unsafe {
            db_va_supports(index, encode as i32, driver.as_mut_ptr(), driver.len() as i32)
        };
        if answer < 0 {
            info!("No VA-API device — is there a VA-API driver for this GPU?");
            let _ = DRIVER.set(None);
            return false;
        }
        let name = message(&driver);
        if DRIVER.set(Some(name.clone())).is_ok() {
            info!("VA-API device: {name}");
        }
        debug!(
            "VA-API {} {}: {}",
            codec.label(),
            if encode { "encode" } else { "decode" },
            answer == 1
        );
        answer == 1
    })
}

/// Whether this machine's GPU decodes `codec` through VA-API.
pub fn can_decode(codec: VideoCodec) -> bool {
    supports(codec, false)
}

/// Whether this machine's GPU encodes `codec` through VA-API, with an encoder
/// for it in this build's FFmpeg.
pub fn can_encode(codec: VideoCodec) -> bool {
    supports(codec, true)
}

/// What the VA-API driver calls itself, once anything has asked it.
pub fn driver_name() -> Option<String> {
    supports(VideoCodec::H264, false);
    DRIVER.get().cloned().flatten()
}

// ── Decode ───────────────────────────────────────────────────────────────

/// A VA-API decoder for one codec.
pub struct VaapiDecoder {
    handle: *mut DbVaDecoder,
    /// What libavcodec and the driver call it, for the stats overlay.
    pub name: String,
}

// SAFETY: the handle is owned solely here and driven from one thread at a
// time — the guest's receive task.
unsafe impl Send for VaapiDecoder {}

impl VaapiDecoder {
    pub fn for_codec(codec: VideoCodec) -> Result<Self> {
        super::ffmpeg_runtime::require()?;
        anyhow::ensure!(
            super::ffmpeg_runtime::vaapi(),
            "libva is not installed, so there is no VA-API"
        );
        let index = codec_index(codec)
            .with_context(|| format!("{} is not a VA-API codec", codec.label()))?;
        let mut err = [0u8; 256];
        // SAFETY: the library and libva are loaded (checked above); the error
        // buffer is valid for its stated length; the handle is checked.
        let handle = unsafe { db_va_decoder_new(index, err.as_mut_ptr(), err.len() as i32) };
        anyhow::ensure!(!handle.is_null(), "{}", message(&err));
        // SAFETY: a live handle.
        let name = c_string(unsafe { db_va_decoder_name(handle) });
        info!("{} decoding on the GPU via {name}", codec.label());
        Ok(Self { handle, name })
    }

    /// Decode one access unit.
    ///
    /// `Ok(None)` when it produced no picture yet — a parameter set, or a
    /// decoder still filling its pipeline. Normal, and not an error.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        if packet.is_empty() {
            return Ok(None);
        }
        // SAFETY: a live handle and a slice valid for its length.
        let rc = unsafe { db_va_decoder_send(self.handle, packet.as_ptr(), packet.len() as i32) };
        anyhow::ensure!(rc >= 0, "the VA-API decoder refused a packet (ffmpeg error {rc})");

        let mut latest = None;
        loop {
            let (mut y, mut uv) = (std::ptr::null(), std::ptr::null());
            let (mut y_stride, mut uv_stride, mut width, mut height) = (0, 0, 0, 0);
            // SAFETY: a live handle and valid out-pointers.
            let rc = unsafe {
                db_va_decoder_receive(
                    self.handle,
                    &mut y,
                    &mut y_stride,
                    &mut uv,
                    &mut uv_stride,
                    &mut width,
                    &mut height,
                )
            };
            if rc == 0 {
                break;
            }
            anyhow::ensure!(rc > 0, "VA-API decode failed (ffmpeg error {rc})");
            anyhow::ensure!(
                (1..=u16::MAX as i32).contains(&width) && (1..=u16::MAX as i32).contains(&height),
                "the decoder produced a {width}x{height} picture"
            );
            let (w, h) = (width as usize, height as usize);
            let (y_stride, uv_stride) = (y_stride as usize, uv_stride as usize);
            // SAFETY: the shim hands back NV12 planes of this size and pitch,
            // valid until the next receive — and they are converted before it.
            let (y_plane, uv_plane) = unsafe {
                (
                    std::slice::from_raw_parts(y, y_stride * (h - 1) + w),
                    std::slice::from_raw_parts(uv, uv_stride * (h / 2).saturating_sub(1) + w),
                )
            };
            let mut rgba = vec![0u8; w * h * 4];
            crate::yuv::nv12_planes_to_rgba(
                y_plane,
                y_stride,
                uv_plane,
                uv_stride,
                width as u16,
                height as u16,
                &mut rgba,
            );
            // A packet normally yields one picture. Should it yield more, the
            // newest is the one to show; the older ones are already late.
            latest = Some(DecodedPicture { width: w, height: h, rgba });
        }
        Ok(latest)
    }
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        // SAFETY: the handle is live and freed exactly once.
        unsafe { db_va_decoder_free(self.handle) };
    }
}

// ── Encode ───────────────────────────────────────────────────────────────

/// A bitrate change smaller than this fraction is not worth a keyframe.
const REBUILD_FRACTION: f64 = 0.2;

enum Backend {
    /// Nothing built yet, or the last one was dropped to be rebuilt.
    Idle,
    Hardware { handle: *mut DbVaEncoder, width: u16, height: u16, kbps: u32 },
    /// OpenH264, for `Automatic` on a GPU that will not encode H.264.
    Software(Box<H264Encoder>),
}

/// A VA-API encoder, rebuilt as the frame size and bitrate require.
pub struct VaapiEncoder {
    codec: VideoCodec,
    config: EncodeConfig,
    /// Whether H.264 may fall back to OpenH264: `Automatic`, never `Hardware`.
    allow_fallback: bool,
    backend: Backend,
    name: String,
    nv12: Vec<u8>,
    keyframe: bool,
    sequence: u32,
}

// SAFETY: the encoder handle is owned solely here; every method that touches it
// takes `&mut self`, so it is never used from two threads at once.
unsafe impl Send for VaapiEncoder {}
unsafe impl Sync for VaapiEncoder {}

impl VaapiEncoder {
    /// Build an encoder for `config`'s codec, failing if this GPU cannot.
    ///
    /// The encoder itself is made on the first frame, when its size is known;
    /// this checks only that one can be.
    pub fn new(config: &EncodeConfig, allow_fallback: bool) -> Result<Self> {
        // The codec as given: `create_encoder` has already resolved it, and
        // resolving again here would quietly turn a refused H.265 into H.264.
        let codec = config.codec;
        super::ffmpeg_runtime::require()?;
        let hardware = super::ffmpeg_runtime::vaapi() && can_encode(codec);
        let allow_fallback = allow_fallback && codec == VideoCodec::H264;
        anyhow::ensure!(
            hardware || allow_fallback,
            "this GPU has no VA-API {} encoder",
            codec.label()
        );
        let mut encoder = Self {
            codec,
            config: config.clone(),
            allow_fallback,
            backend: Backend::Idle,
            name: driver_name().unwrap_or_else(|| "VA-API".to_string()),
            nv12: Vec::new(),
            keyframe: false,
            sequence: 0,
        };
        if !hardware {
            encoder.fall_back("no VA-API H.264 encoder")?;
        }
        Ok(encoder)
    }

    /// What it is encoding on, for a log line.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn release(&mut self) {
        if let Backend::Hardware { handle, .. } = std::mem::replace(&mut self.backend, Backend::Idle)
        {
            // SAFETY: the handle is live and freed exactly once.
            unsafe { db_va_encoder_free(handle) };
        }
    }

    fn fall_back(&mut self, why: &str) -> Result<()> {
        anyhow::ensure!(self.allow_fallback, "{why}");
        info!("H.264 on the processor instead of the GPU ({why})");
        self.release();
        self.backend = Backend::Software(Box::new(H264Encoder::new(&self.config)?));
        self.name = "OpenH264".to_string();
        Ok(())
    }

    fn target_kbps(&self, width: u16, height: u16) -> u32 {
        match self.config.target_bitrate_kbps {
            0 => default_bitrate_bps(width, height, self.config.fps) / 1000,
            kbps => kbps,
        }
    }

    /// The hardware encoder for this size, built if it is not already.
    fn hardware_for(&mut self, width: u16, height: u16) -> Result<*mut DbVaEncoder> {
        if let Backend::Hardware { handle, width: w, height: h, .. } = self.backend {
            if (w, h) == (width, height) {
                return Ok(handle);
            }
            self.release();
        }
        let kbps = self.target_kbps(width, height);
        let index = codec_index(self.codec).expect("only VA-API codecs reach here");
        let mut err = [0u8; 256];
        // SAFETY: the library and libva are loaded (checked in `new`); the
        // error buffer is valid for its stated length; the handle is checked.
        let handle = unsafe {
            db_va_encoder_new(
                index,
                width as i32,
                height as i32,
                self.config.fps.max(1) as i32,
                kbps as i32,
                self.config.keyframe_interval as i32,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        anyhow::ensure!(!handle.is_null(), "{}", message(&err));
        // SAFETY: a live handle.
        self.name = c_string(unsafe { db_va_encoder_name(handle) });
        info!("{} encoding on the GPU via {} at {kbps} kbit/s", self.codec.label(), self.name);
        self.backend = Backend::Hardware { handle, width, height, kbps };
        Ok(handle)
    }
}

impl VideoEncoder for VaapiEncoder {
    fn encode_frame(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<EncodedFrame> {
        let (w, h) = (width as usize, height as usize);
        let nv12_len = crate::yuv::nv12_len(width, height);
        // Taken out so the conversion can borrow `self` mutably below.
        let mut scratch = std::mem::take(&mut self.nv12);
        let nv12: &[u8] = match format {
            PixelFormat::Nv12 => {
                anyhow::ensure!(pixels.len() >= nv12_len, "NV12 frame is short");
                &pixels[..nv12_len]
            }
            PixelFormat::Bgra => {
                anyhow::ensure!(pixels.len() >= w * h * 4, "BGRA frame is short");
                crate::yuv::bgra_to_nv12(pixels, width, height, &mut scratch);
                &scratch
            }
        };

        let force = std::mem::take(&mut self.keyframe);
        let result = (|| -> Result<(Vec<u8>, bool)> {
            if !matches!(self.backend, Backend::Software(_)) {
                match self.hardware_for(width, height) {
                    Ok(handle) => {
                        let (mut out, mut len, mut key) = (std::ptr::null(), 0, 0);
                        // SAFETY: a live handle, an NV12 frame of the size it
                        // was built for, and valid out-pointers. The output is
                        // copied before the next call can overwrite it.
                        let rc = unsafe {
                            db_va_encoder_encode(
                                handle,
                                nv12.as_ptr(),
                                force as i32,
                                &mut out,
                                &mut len,
                                &mut key,
                            )
                        };
                        anyhow::ensure!(rc >= 0, "VA-API encode failed (ffmpeg error {rc})");
                        let data = if out.is_null() || len <= 0 {
                            Vec::new()
                        } else {
                            // SAFETY: the shim's buffer, `len` bytes, valid
                            // until the next encode call.
                            unsafe { std::slice::from_raw_parts(out, len as usize) }.to_vec()
                        };
                        return Ok((data, key != 0));
                    }
                    Err(e) => self.fall_back(&format!("{e:#}"))?,
                }
            }
            let Backend::Software(encoder) = &mut self.backend else {
                unreachable!("fall_back leaves a software encoder or returns an error")
            };
            if force {
                encoder.request_keyframe();
            }
            encoder.encode_nv12(nv12, width, height)
        })();
        self.nv12 = scratch;
        let (data, is_keyframe) = result?;

        self.sequence = self.sequence.wrapping_add(1);
        Ok(EncodedFrame {
            sequence_number: self.sequence,
            width,
            height,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            is_keyframe,
            data: Bytes::from(data),
        })
    }

    fn preferred_format(&self) -> PixelFormat {
        PixelFormat::Nv12
    }

    fn codec(&self) -> VideoCodec {
        match self.backend {
            Backend::Software(_) => VideoCodec::H264,
            _ => self.codec,
        }
    }

    fn reconfigure(&mut self, config: &EncodeConfig) -> Result<()> {
        if self.config == *config {
            return Ok(());
        }
        let structural = EncodeConfig {
            target_bitrate_kbps: config.target_bitrate_kbps,
            fps: config.fps,
            ..self.config.clone()
        } != *config;
        self.config = config.clone();

        match &mut self.backend {
            Backend::Software(encoder) => encoder.reconfigure(config),
            Backend::Hardware { width, height, kbps, .. } => {
                let (width, height, kbps) = (*width, *height, *kbps);
                let wanted = self.target_kbps(width, height);
                let change = (wanted as f64 - kbps as f64).abs() / kbps.max(1) as f64;
                if structural || change > REBUILD_FRACTION {
                    debug!("Rebuilding the VA-API encoder for {wanted} kbit/s (was {kbps})");
                    self.release();
                }
            }
            Backend::Idle => {}
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.keyframe = true;
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        self.release();
    }
}

/// Build a VA-API encoder the way `create_encoder` wants one: hardware, or for
/// `Automatic` H.264 the processor when the GPU will not.
pub fn create(config: &EncodeConfig) -> Result<Box<dyn VideoEncoder>> {
    let allow_fallback = config.backend == EncoderBackend::Auto;
    Ok(Box::new(VaapiEncoder::new(config, allow_fallback)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_hardware_codecs_have_a_va_api_number() {
        assert_eq!(codec_index(VideoCodec::H264), Some(0));
        assert_eq!(codec_index(VideoCodec::H265), Some(1));
        assert_eq!(codec_index(VideoCodec::Av1), Some(2));
        assert_eq!(codec_index(VideoCodec::Raw), None);
        assert_eq!(codec_index(VideoCodec::DeltaZstd), None);
    }

    /// Asking a machine with no GPU must answer no rather than fail: the
    /// settings window asks on every machine.
    #[test]
    fn asking_what_the_gpu_can_do_never_fails() {
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            let _ = can_decode(codec);
            let _ = can_encode(codec);
        }
        let _ = driver_name();
    }

    /// `Automatic` H.264 always gets an encoder: the GPU's if it has one, the
    /// processor's if not. Run where there is no VA-API at all, this proves
    /// the fallback produces a stream.
    #[test]
    fn automatic_h264_always_encodes() {
        if !crate::ffmpeg_runtime::available() {
            eprintln!("no media library in this build — not exercised");
            return;
        }
        let config = EncodeConfig { codec: VideoCodec::H264, ..EncodeConfig::default() };
        let mut encoder = VaapiEncoder::new(&config, true).expect("Automatic must not fail");
        let (w, h) = (320u16, 240u16);
        let frame = vec![0x60u8; w as usize * h as usize * 4];
        let mut produced = 0;
        for _ in 0..5 {
            produced += encoder.encode_frame(&frame, w, h, PixelFormat::Bgra).unwrap().data.len();
        }
        assert!(produced > 0, "five frames produced nothing");
        assert_eq!(encoder.codec(), VideoCodec::H264);
    }

    /// A GPU that cannot encode H.265 says so instead of pretending.
    #[test]
    fn a_codec_the_gpu_cannot_encode_is_refused() {
        if !crate::ffmpeg_runtime::available() || can_encode(VideoCodec::H265) {
            eprintln!("this machine can encode H.265, or has no media library — not exercised");
            return;
        }
        let config = EncodeConfig { codec: VideoCodec::H265, ..EncodeConfig::default() };
        assert!(VaapiEncoder::new(&config, true).is_err());
    }
}
