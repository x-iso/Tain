//! Video encoding pipeline for screen frames.
//!
//! The codec surface here is the full set a remote desktop realistically needs.
//! [`VideoCodec::H264`], [`VideoCodec::DeltaZstd`] and [`VideoCodec::Raw`] have
//! backends; the rest are declared with an honest [`Availability`] so the UI can
//! show what is coming without pretending it works.
//! [`EncodeConfig::effective_codec`] resolves whatever was requested down to
//! something that actually runs, and the host advertises *that* to the guest —
//! never the aspiration.

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::Mutex as StdMutex;

use super::screen_capture::PixelFormat;
use tracing::*;

/// Whether a codec can actually be used right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Implemented and selectable.
    Available,
    /// Declared, not yet implemented. Falls back to raw.
    Planned,
}

/// Video codecs the pipeline knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// Lossless XOR delta against the previous frame, then zstd. Cheap, exact,
    /// and extremely effective on a *static* desktop — but the moment anything
    /// moves the delta stops being zeroes and the cost explodes.
    DeltaZstd,
    /// Uncompressed BGRA. Zero encode latency, enormous bandwidth. LAN only.
    Raw,
    /// The workhorse, and the default: an order of magnitude smaller than the
    /// lossless path on real desktop use, and low enough latency to stay
    /// interactive.
    #[default]
    H264,
    /// Roughly 30-50% smaller than H.264 at equal quality, for more encode cost.
    H265,
    /// Best compression of the set; hardware support is recent and patchy.
    Av1,
    /// Royalty-free, widely supported in software, higher latency than H.264.
    Vp9,
}

impl VideoCodec {
    pub const ALL: [Self; 6] =
        [Self::DeltaZstd, Self::Raw, Self::H264, Self::H265, Self::Av1, Self::Vp9];

    pub fn label(self) -> &'static str {
        match self {
            Self::DeltaZstd => "Lossless (delta + zstd)",
            Self::Raw => "Raw BGRA",
            Self::H264 => "H.264 / AVC",
            Self::H265 => "H.265 / HEVC",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
        }
    }

    /// Short note on what this codec is for, shown next to the choice.
    pub fn description(self) -> &'static str {
        match self {
            Self::DeltaZstd => {
                "Pixel-exact and CPU-only. Cheap on a still screen, very expensive once it moves."
            }
            Self::Raw => "No compression. Lowest latency, needs a fast local network.",
            Self::H264 => "Best all-round choice. Software encoder, works on any machine.",
            Self::H265 => "Smaller than H.264 at the same quality, costs more to encode.",
            Self::Av1 => "Smallest streams. Needs recent hardware to stay low-latency.",
            Self::Vp9 => "Royalty-free. Software encoding is slower than H.264.",
        }
    }

    pub fn availability(self) -> Availability {
        match self {
            Self::H264 | Self::DeltaZstd | Self::Raw => Availability::Available,
            // Implemented, but only where the *machine* can encode it — see
            // [`Self::is_encodable_here`]. `Available` is about this build; the
            // hardware question is answered separately because the answer
            // differs between the two ends of the same session.
            // As H.265: the code exists, and whether this machine can encode it
            // is asked separately. AV1 encoders are newer hardware still — Arc,
            // Ada, RDNA3 and up — so most machines will answer no and advertise
            // H.264 instead.
            Self::H265 | Self::Av1 => Availability::Available,
            _ => Availability::Planned,
        }
    }

    /// Whether this machine has hardware that can encode this codec.
    ///
    /// Distinct from [`Self::availability`], which only says whether the code
    /// exists. HEVC encoders are a GPU feature: common on anything from the last
    /// decade and absent on plenty of what is still in use, and there is no
    /// software HEVC encoder here to fall back to. So the answer is asked of the
    /// machine rather than assumed, and a host that cannot encode it advertises
    /// H.264 instead — see [`EncodeConfig::effective_codec`].
    /// Worked out once. `effective_codec` calls this, and `effective_codec` is
    /// called per frame — enumerating Media Foundation transforms that often
    /// turned a cheap accessor into a COM round trip on the encode path, which
    /// was slow enough to hang a test run before it was noticed.
    pub fn is_encodable_here(self) -> bool {
        match self {
            Self::H265 => {
                static HEVC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *HEVC.get_or_init(|| Self::H265.ask_the_machine("HEVC"))
            }
            Self::Av1 => {
                static AV1: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *AV1.get_or_init(|| Self::Av1.ask_the_machine("AV1"))
            }
            other => other.is_available(),
        }
    }

    /// Whether Media Foundation offers an encoder for this codec here.
    ///
    /// Asked of the machine rather than assumed, and only once per codec — the
    /// caller caches it, because this is a COM enumeration and `effective_codec`
    /// is on the encode path.
    fn ask_the_machine(self, label: &str) -> bool {
        #[cfg(target_os = "windows")]
        {
            let found = !super::codec_mf::available_encoders_for(self).is_empty();
            debug!("{label} hardware encoder present: {found}");
            found
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = label;
            false
        }
    }

    /// Whether decoding needs state carried between frames.
    pub fn is_interframe(self) -> bool {
        !matches!(self, Self::Raw)
    }

    /// Whether the codec throws information away to hit its bitrate.
    ///
    /// Drives whether quality and bitrate controls mean anything: the lossless
    /// path ignores both.
    pub fn is_lossy(self) -> bool {
        !matches!(self, Self::Raw | Self::DeltaZstd)
    }

    pub fn is_available(self) -> bool {
        self.availability() == Availability::Available
    }

    /// Whether this codec compresses at all — raw ignores bitrate and quality.
    pub fn is_compressed(self) -> bool {
        !matches!(self, Self::Raw)
    }

    /// Name used in the `SessionStart` handshake.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::DeltaZstd => "delta-zstd",
            Self::Raw => "raw",
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
        }
    }

    /// Resolve a codec from the name carried in `SessionStart`.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.wire_name() == name)
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Where the encoding work should happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderBackend {
    /// Prefer hardware, fall back to software.
    #[default]
    Auto,
    /// Require a GPU encoder (NVENC, Quick Sync, AMF, Media Foundation).
    Hardware,
    /// Force a CPU encoder. Predictable, but competes with whatever the host is doing.
    Software,
}

impl EncoderBackend {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Hardware, Self::Software];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Hardware => "Hardware (GPU)",
            Self::Software => "Software (CPU)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "Use the GPU when available, otherwise the CPU",
            Self::Hardware => "GPU only — lowest latency, fails if unsupported",
            Self::Software => "CPU only — works anywhere, competes with the host's own work",
        }
    }
}

/// How long a damaged picture may persist before a keyframe repairs it.
///
/// The whole trade in one number. Short means a bandwidth and decode spike on a
/// fixed cadence, which is visible as a bump on an otherwise smooth stream.
/// Long means corruption stays on screen. Thirty seconds is far enough apart
/// that the spike is lost in ordinary variation, and near enough that damage is
/// a blemish rather than a broken session.
///
/// Expressed in seconds and converted, because the interval the encoder takes is
/// in frames and a frame is not a fixed amount of time — the old constant was
/// two seconds at 60 fps and five at 24.
pub const KEYFRAME_SECONDS: u32 = 30;

/// Where to sit on the quality-versus-latency trade **when something has to
/// give**.
///
/// This is not a quality setting and does not lower anything by itself. Every
/// position streams the full resolution and frame rate the user asked for on a
/// link and a machine that can carry them. What it decides is what the
/// [`super::adaptive::AdaptiveController`] sacrifices first once a stage has
/// measurably fallen behind, and how quickly it reacts:
///
/// - towards **quality**, the frame rate sags and the pixels stay;
/// - towards **latency**, pixels go and the frame rate holds.
///
/// A single knob, because the underlying trades are not independent — asking
/// for resolution, frame rate and bitrate separately hands the user a
/// constrained optimisation whose answer depends on measurements only the app
/// can see.
///
/// 0 is "keep it looking right", 100 is "keep it responding".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyPriority(u8);

impl LatencyPriority {
    /// Middle of the range.
    pub const BALANCED: Self = Self(50);
    /// Hold the picture; let the frame rate fall.
    pub const QUALITY: Self = Self(0);
    /// Hold the frame rate; let the picture soften.
    pub const LATENCY: Self = Self(100);

    pub fn new(value: u8) -> Self {
        Self(value.min(100))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    /// Whether resolution is given up before frame rate.
    ///
    /// The dividing line of the whole setting. Above the midpoint, a stream in
    /// trouble sheds pixels to keep moving; below it, it keeps the pixels and
    /// moves less.
    pub fn sheds_pixels_first(self) -> bool {
        self.0 > 50
    }

    /// How far the controller may reduce resolution before refusing to go on.
    ///
    /// Even the quality end has a floor below native, because a stream stuck at
    /// its frame-rate floor and still failing has to give something — holding
    /// every pixel of a picture that updates twice a second serves nobody.
    pub fn min_scale(self) -> super::screen_capture::QualityScale {
        use super::screen_capture::QualityScale;
        match self.0 {
            0..=33 => QualityScale::High,
            34..=66 => QualityScale::Medium,
            _ => QualityScale::Low,
        }
    }

    /// Frame rate the controller will not drop below.
    ///
    /// The latency end holds a high floor because responsiveness is the thing
    /// being protected; the quality end tolerates a slideshow to keep detail.
    pub fn min_fps(self) -> u32 {
        match self.0 {
            0..=33 => 8,
            34..=66 => 15,
            67..=85 => 30,
            _ => 45,
        }
    }

    /// Consecutive strained windows before the controller acts.
    ///
    /// Latency-first reacts sooner because a stutter is the symptom it exists to
    /// prevent; quality-first waits, because a brief dip costs less than a
    /// visible drop in resolution that then has to be undone.
    pub fn strain_tolerance(self) -> u32 {
        match self.0 {
            0..=33 => 4,
            34..=66 => 3,
            _ => 2,
        }
    }

    /// Consecutive clean windows before quality is taken back.
    ///
    /// Longer than [`Self::strain_tolerance`] on purpose: recovering too eagerly
    /// on a marginal link produces a stream that visibly pumps up and down,
    /// which is more distracting than sitting one step low.
    pub fn recovery_patience(self) -> u32 {
        match self.0 {
            0..=33 => 6,
            34..=66 => 7,
            _ => 8,
        }
    }

    /// Quantiser bias, 0-100, fed to the encoder's quality target.
    ///
    /// The one thing set up front rather than adaptively: it is the encoder's
    /// standing instruction about how much detail a frame is worth, and unlike
    /// resolution it can be held constant while bitrate moves around it. The
    /// range is deliberately narrow — both ends have to look good on a healthy
    /// link, because that is where a stream spends most of its life.
    pub fn encode_quality(self) -> u8 {
        // 85 at the quality end down to 60 at the latency end.
        85_u8.saturating_sub((self.0 as u16 * 25 / 100) as u8)
    }

    /// Frames between keyframes, at 60 fps. See [`KEYFRAME_SECONDS`].
    ///
    /// # This was zero, and zero was wrong
    ///
    /// The argument for zero was that keyframes are insurance against losing a
    /// frame in transit, and nothing here can lose one: the stream is reliable
    /// and ordered, and the host only discards frames *before* the encoder where
    /// no inter-frame state exists yet. All of that is still true.
    ///
    /// What it missed is that a decoder can lose sync without anything being
    /// lost. A driver hiccup, a surface the decoder reused too early, a
    /// parameter set it read differently — the frame arrives intact and the
    /// picture still comes out wrong. Reported as blocky corrupted regions that,
    /// with no keyframe coming, **stayed** instead of clearing after a second or
    /// two the way they used to.
    ///
    /// The guest asks for a keyframe when its decoder returns an *error*. This
    /// covers the case where it returns a picture and the picture is damaged,
    /// which it cannot detect and neither can the host.
    ///
    /// So: rare rather than absent. Every few seconds is a spike on a schedule;
    /// every half a minute is a repair nobody notices.
    pub fn keyframe_interval(self) -> u32 {
        KEYFRAME_SECONDS * 60
    }

    /// Bits per pixel per second to ask for when no bitrate limit is set.
    ///
    /// # Why the ceiling moves with the slider
    ///
    /// It did not. The ceiling came from one constant — 0.07 bits a pixel — so
    /// "highest quality" on a link with headroom to spare asked for exactly the
    /// same bitrate as every other position, and the adaptive controller could
    /// only ever go *down* from there. Reported as a picture that stayed
    /// slightly lossy while the bandwidth sat unused, which is precisely what a
    /// fixed formula does on a fast link.
    ///
    /// The controller still lowers this the moment anything cannot keep up, so
    /// the quality end is not a promise of bandwidth — it is permission to use
    /// it when it is there.
    pub fn quality_bits_per_pixel(self) -> f64 {
        match self.0 {
            0..=33 => 0.15,
            34..=66 => 0.09,
            _ => 0.06,
        }
    }

    /// Smallest audio buffer this position will settle at, in milliseconds.
    ///
    /// # Why buffering belongs on this slider
    ///
    /// A jitter buffer converts variation in arrival time into latency. That is
    /// the same trade the rest of this type makes, so it is the same knob: at
    /// the latency end the buffer stays tight and the occasional hiccup is
    /// audible; at the quality end it is allowed to grow until the sound is
    /// continuous.
    ///
    /// This is only where it *starts*. See [`Self::audio_buffer_ceiling_ms`] —
    /// the buffer earns its size by underrunning, and gives it back when the
    /// link settles.
    pub fn audio_buffer_floor_ms(self) -> u32 {
        match self.0 {
            0..=33 => 40,
            34..=66 => 25,
            _ => 15,
        }
    }

    /// The most audio latency this position will accept to stay continuous.
    ///
    /// Reached only on a link that keeps breaking a smaller buffer. A stream on
    /// a healthy link never gets near it, because the buffer decays back towards
    /// [`Self::audio_buffer_floor_ms`] whenever nothing goes wrong.
    pub fn audio_buffer_ceiling_ms(self) -> u32 {
        match self.0 {
            0..=33 => 300,
            34..=66 => 180,
            _ => 80,
        }
    }

    /// Short description of what this position actually does.
    pub fn summary(self) -> &'static str {
        match self.0 {
            0..=33 => {
                "Holds the picture. If the link or the hardware falls behind, the \
                 frame rate drops before the resolution does, and sound is \
                 buffered generously so it stays continuous."
            }
            34..=66 => {
                "Balanced. Trouble is answered with bitrate first, then whichever \
                 of detail and smoothness is cheaper to give up."
            }
            67..=85 => {
                "Favours responsiveness. Resolution softens before the frame rate \
                 is allowed to fall."
            }
            _ => {
                "Holds the frame rate. Detail is spent freely, and sound is \
                 buffered as little as possible, to keep everything immediate."
            }
        }
    }
}

impl Default for LatencyPriority {
    fn default() -> Self {
        Self::BALANCED
    }
}

/// How bitrate is allocated over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateControl {
    /// Hold a steady bitrate. Predictable bandwidth, quality varies with motion.
    #[default]
    Constant,
    /// Spend more on complex frames, up to the limit. Better quality per byte.
    Variable,
    /// Target a quality level and let bitrate follow. Can spike on motion.
    ConstantQuality,
}

impl RateControl {
    pub const ALL: [Self; 3] = [Self::Constant, Self::Variable, Self::ConstantQuality];

    pub fn label(self) -> &'static str {
        match self {
            Self::Constant => "Constant bitrate",
            Self::Variable => "Variable bitrate",
            Self::ConstantQuality => "Constant quality",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Constant => "Steady bandwidth; quality dips when the screen moves a lot",
            Self::Variable => "Better quality per byte, bandwidth varies",
            Self::ConstantQuality => "Pick a quality and let bandwidth follow — can spike",
        }
    }

    /// Whether this mode is driven by a bitrate target rather than a quality target.
    pub fn uses_bitrate(self) -> bool {
        !matches!(self, Self::ConstantQuality)
    }
}

/// Encoding configuration.
/// Which H.264 profile to encode.
///
/// # Why this is a setting rather than a constant
///
/// Because the right answer depends on the *guest*, and the host cannot always
/// tell.
///
/// It was fixed at Baseline because OpenH264 — what a guest without the ffmpeg
/// runtime decodes with — could not read anything else. Baseline is also the
/// profile **Intel Quick Sync stalls on**: the transform accepts the type,
/// begins streaming, and then never asks for a frame again. No error, no output,
/// just a session that appears to freeze. That is a real report from a real
/// machine, and it means those hosts had no working profile at all.
///
/// So it is offered as a choice. The default stays where it was, because a
/// guest on the CPU decoder is the case that cannot recover, and
/// `openh264_decodes_every_profile_this_can_produce` measures exactly how badly
/// it fails: Main and High decode roughly three packets in ten, which is not a
/// stream, it is a slideshow of artefacts.
///
/// Anyone decoding on their GPU — which is everyone with the ffmpeg runtime,
/// meaning every standard build — can raise it and should, particularly on
/// Intel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Profile {
    /// No CABAC, no 8x8 transform. Largest bitstream, and the only profile the
    /// CPU fallback decoder reads — which is why it is the default.
    ///
    /// Also the profile Intel Quick Sync has been seen to stall on.
    #[default]
    Baseline,
    /// CABAC and better prediction, still no B-frames at these settings.
    ///
    /// Meaningfully smaller than Baseline at the same quality, and what Intel's
    /// encoder is actually built around. Needs a guest decoding on its GPU.
    Main,
    /// Main plus the 8x8 transform. A few percent smaller again on desktop
    /// content. Needs a guest decoding on its GPU.
    High,
}

impl H264Profile {
    pub const ALL: [Self; 3] = [Self::Baseline, Self::Main, Self::High];

    /// The Media Foundation constant, from `eAVEncH264VProfile`.
    pub fn to_mf(self) -> u32 {
        match self {
            Self::Baseline => 66,
            Self::Main => 77,
            Self::High => 100,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Baseline => "Baseline",
            Self::Main => "Main",
            Self::High => "High",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Baseline => {
                "Works with every guest, including one decoding on its CPU. \
                 Largest bitstream, and some Intel encoders stall on it."
            }
            Self::Main => {
                "Smaller at the same quality, and the fix if an Intel encoder \
                 stalls. The guest must be decoding on its GPU."
            }
            Self::High => {
                "A few percent smaller again. The guest must be decoding on its GPU."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncodeConfig {
    pub codec: VideoCodec,
    pub backend: EncoderBackend,
    /// H.264 profile to ask the encoder for.
    pub profile: H264Profile,
    /// Substring of the encoder's name to prefer, e.g. "NVIDIA".
    ///
    /// Empty means "whichever the system offers first". Matched loosely because
    /// these names are vendor prose rather than identifiers.
    pub preferred_encoder: String,
    pub rate_control: RateControl,
    /// Target/ceiling bitrate in kbps. 0 means unlimited.
    pub target_bitrate_kbps: u32,
    /// Quality target 0-100 for [`RateControl::ConstantQuality`].
    pub quality: u8,
    /// Frames between keyframes. 0 means "only when the guest asks".
    pub keyframe_interval: u32,
    pub fps: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::default(),
            profile: H264Profile::default(),
            preferred_encoder: String::new(),
            backend: EncoderBackend::default(),
            rate_control: RateControl::default(),
            target_bitrate_kbps: 0,
            quality: 70,
            keyframe_interval: KEYFRAME_SECONDS * 60,
            fps: 30,
        }
    }
}

impl EncodeConfig {
    /// The codec that will actually run, after unimplemented ones fall back.
    ///
    /// Use this — not `codec` — when telling the guest what to expect.
    pub fn effective_codec(&self) -> VideoCodec {
        if self.codec.is_available() && self.codec.is_encodable_here() {
            self.codec
        } else {
            // Fall back to H.264, not raw: an unimplemented choice should
            // degrade to something usable over a network, not to 400 MB/s. The
            // planned codecs are all lossy, so H.264 is the nearest neighbour.
            VideoCodec::H264
        }
    }

    /// True when the requested codec is not the one that will run.
    pub fn is_falling_back(&self) -> bool {
        self.codec != self.effective_codec()
    }

    /// Clamp to sane values, so a hand-edited config cannot poison the encoder.
    pub fn sanitize(&mut self) {
        self.fps = self.fps.clamp(1, 240);
        self.quality = self.quality.min(100);
        self.keyframe_interval = self.keyframe_interval.min(3600);
        if self.target_bitrate_kbps != 0 {
            self.target_bitrate_kbps = self.target_bitrate_kbps.clamp(100, 500_000);
        }
    }
}

/// Encoded video frame ready to be sent over the network.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub sequence_number: u32,
    pub width: u16,
    pub height: u16,
    pub timestamp_ms: u64,
    /// True if this frame stands alone (IDR for inter-frame codecs; always true for raw).
    pub is_keyframe: bool,
    pub data: Bytes,
}

/// Trait for video encoders. Implementations must be thread-safe.
pub trait VideoEncoder: Send + Sync {
    /// Encode one raw frame laid out as `format`.
    ///
    /// The format travels with the pixels rather than being configured once,
    /// because the failure mode of getting it wrong is silent: NV12 read as
    /// BGRA is a quarter of the picture in the wrong colours, and every length
    /// check still passes. An encoder that cannot take what it was handed must
    /// say so.
    fn encode_frame(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<EncodedFrame>;

    /// What this encoder would rather be given, if the capture side can produce it.
    ///
    /// Only an advisory: [`encode_frame`](Self::encode_frame) must still handle
    /// BGRA, which is the only format every capture backend can produce.
    fn preferred_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }

    /// The codec this encoder actually produces.
    fn codec(&self) -> VideoCodec;

    /// Change bitrate or frame rate mid-session.
    ///
    /// The adaptive controller calls this when it lowers or restores the
    /// bitrate. Implementations rebuild lazily on the next frame rather than
    /// here, so a reconfiguration cannot fail in the middle of a session — and
    /// the rebuild's keyframe is wanted anyway, since the following frames are
    /// encoded to a different budget.
    fn reconfigure(&mut self, _config: &EncodeConfig) -> Result<()> {
        Ok(())
    }

    /// Make the next frame stand alone, because a guest cannot decode any more.
    ///
    /// # Why this exists rather than a timer
    ///
    /// Periodic keyframes are a defence against a *lossy* transport: lose a
    /// packet, and every frame after it is undecodable until the next one that
    /// stands alone. This does not run over a lossy transport. QUIC streams are
    /// reliable and ordered, the host only ever discards frames *before* the
    /// encoder — where no inter-frame state exists yet — and the guest decodes
    /// every frame it receives even when it later replaces the picture without
    /// drawing it. Nothing can go missing in between.
    ///
    /// So a keyframe every few seconds bought nothing and cost a burst several
    /// times the size of a normal frame, which on a constrained link is a
    /// visible hitch on a fixed period. Recovery is driven by the one thing that
    /// can actually go wrong instead: the guest's decoder failing, which the
    /// guest reports and which this answers immediately rather than up to a
    /// keyframe interval later.
    ///
    /// Default is a no-op, for encoders whose every frame already stands alone.
    fn request_keyframe(&mut self) {}
}

// ── Raw BGRA passthrough (no encoding: zero latency, highest bandwidth) ─

pub struct RawBgraEncoder {
    sequence_counter: StdMutex<u32>,
}

impl RawBgraEncoder {
    pub fn new(_config: &EncodeConfig) -> Result<Self> {
        Ok(Self { sequence_counter: StdMutex::new(0) })
    }
}

impl VideoEncoder for RawBgraEncoder {
    fn encode_frame(
        &mut self,
        bgra_data: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<EncodedFrame> {
        if format != PixelFormat::Bgra {
            anyhow::bail!("{format:?} input is not supported by this encoder");
        }
        let expected = width as usize * height as usize * 4;
        if bgra_data.len() < expected {
            anyhow::bail!(
                "BGRA buffer too small for {}x{}: got {} bytes, need {}",
                width,
                height,
                bgra_data.len(),
                expected
            );
        }

        let seq = {
            let mut counter = self.sequence_counter.lock().unwrap();
            *counter = counter.wrapping_add(1);
            *counter
        };

        Ok(EncodedFrame {
            sequence_number: seq,
            width,
            height,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            // No inter-frame compression, so every frame stands alone.
            is_keyframe: true,
            data: Bytes::copy_from_slice(&bgra_data[..expected]),
        })
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::Raw
    }
}

// ── Lossless delta + zstd ────────────────────────────────────

/// Wraps [`super::codec_delta::DeltaEncoder`] in the `VideoEncoder` interface.
pub struct DeltaZstdEncoder {
    inner: super::codec_delta::DeltaEncoder,
    sequence_counter: StdMutex<u32>,
}

impl DeltaZstdEncoder {
    pub fn new(config: &EncodeConfig) -> Result<Self> {
        Ok(Self {
            inner: super::codec_delta::DeltaEncoder::new(config.keyframe_interval),
            sequence_counter: StdMutex::new(0),
        })
    }
}

impl VideoEncoder for DeltaZstdEncoder {
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn encode_frame(
        &mut self,
        bgra_data: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<EncodedFrame> {
        if format != PixelFormat::Bgra {
            anyhow::bail!("{format:?} input is not supported by this encoder");
        }
        let expected = width as usize * height as usize * 4;
        if bgra_data.len() < expected {
            anyhow::bail!(
                "BGRA buffer too small for {}x{}: got {} bytes, need {}",
                width,
                height,
                bgra_data.len(),
                expected
            );
        }

        let (data, is_keyframe) = self.inner.encode(&bgra_data[..expected])?;
        let seq = {
            let mut counter = self.sequence_counter.lock().unwrap();
            *counter = counter.wrapping_add(1);
            *counter
        };

        Ok(EncodedFrame {
            sequence_number: seq,
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

    fn codec(&self) -> VideoCodec {
        VideoCodec::DeltaZstd
    }
}

// ── H.264 ────────────────────────────────────────────────────

/// Wraps [`super::codec_h264::H264Encoder`] in the `VideoEncoder` interface.
pub struct H264VideoEncoder {
    inner: super::codec_h264::H264Encoder,
    sequence_counter: StdMutex<u32>,
}

impl H264VideoEncoder {
    pub fn new(config: &EncodeConfig) -> Result<Self> {
        Ok(Self {
            inner: super::codec_h264::H264Encoder::new(config)?,
            sequence_counter: StdMutex::new(0),
        })
    }
}

impl VideoEncoder for H264VideoEncoder {
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn encode_frame(
        &mut self,
        bgra_data: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<EncodedFrame> {
        if format != PixelFormat::Bgra {
            anyhow::bail!("{format:?} input is not supported by this encoder");
        }
        let (data, is_keyframe) = self.inner.encode(bgra_data, width, height)?;
        let seq = {
            let mut counter = self.sequence_counter.lock().unwrap();
            *counter = counter.wrapping_add(1);
            *counter
        };

        Ok(EncodedFrame {
            sequence_number: seq,
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

    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn reconfigure(&mut self, config: &EncodeConfig) -> Result<()> {
        self.inner.reconfigure(config);
        Ok(())
    }
}

/// Bitrate to aim for at a given size and frame rate when none was set.
///
/// Shared with the codec so the adaptive controller's ceiling and the encoder's
/// own default cannot drift apart — a controller that thought the ceiling was
/// 20 Mbit/s while the encoder ran at 8 would report a degradation that never
/// happened.
pub fn default_bitrate_kbps(width: u16, height: u16, fps: u32) -> u32 {
    (super::codec_h264::default_bitrate_bps(width, height, fps) / 1000).max(100)
}

/// The same, but generous or frugal according to the quality slider.
///
/// See [`LatencyPriority::quality_bits_per_pixel`] for why this is not one
/// number for everybody.
pub fn ceiling_bitrate_kbps(
    width: u16,
    height: u16,
    fps: u32,
    priority: LatencyPriority,
) -> u32 {
    let pixels = width as f64 * height as f64 * fps.max(1) as f64;
    let bps = pixels * priority.quality_bits_per_pixel();
    // The same bounds the fixed formula used: enough to be a picture, and not so
    // much that a 4K host asks for more than any link will carry.
    ((bps / 1000.0) as u32).clamp(100, 50_000)
}

/// Name of this machine's hardware H.264 encoder, if it has one.
///
/// Cheap enough to call from the settings UI: it enumerates transforms and
/// activates one, but does not configure or run it.
pub fn hardware_encoder_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        super::codec_mf::probe().map(|info| info.name)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // VideoToolbox and VA-API are the equivalents elsewhere; neither is
        // wired up, and claiming otherwise would make the UI lie.
        None
    }
}

/// The hardware encoder a session with this preference would actually use.
///
/// [`hardware_encoder_name`] answers "does this machine have one", which is a
/// different question once a preference exists: it activates whichever the
/// system lists first, so with a preference set it names the wrong card.
pub fn hardware_encoder_name_for(prefer: &str) -> Option<String> {
    // The probe first, because it is the one that proves a transform can be
    // activated at all. With no preference that is the whole answer.
    let default = hardware_encoder_name()?;
    if prefer.is_empty() {
        return Some(default);
    }
    // The same rule the encoder itself applies, including falling back to the
    // default when the preference names something this machine does not have.
    let wanted = prefer.to_lowercase();
    Some(
        available_hardware_encoders()
            .into_iter()
            .find(|name| name.to_lowercase().contains(&wanted))
            .unwrap_or(default),
    )
}

/// Every hardware H.264 encoder this machine offers, for a picker to list.
///
/// # Why a machine has more than one
///
/// A laptop with an integrated and a discrete GPU registers an encoder for
/// each, and which one gets used is otherwise the system's choice — which is how
/// a machine with an NVIDIA card ended up encoding on the Intel iGPU. Setting
/// [`EncodeConfig::preferred_encoder`] to any part of one of these names picks
/// that one.
///
/// Duplicates are removed. The same name twice is the same choice as far as
/// anyone selecting one is concerned, and the preference matcher cannot tell
/// them apart either.
///
/// Worked out once, for the same reason [`probe`](super::codec_mf::probe) is:
/// this is read from the settings UI, which redraws continuously, and
/// enumerating transforms is a COM round trip. A GPU does not appear mid-run.
pub fn available_hardware_encoders() -> Vec<String> {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| {
            #[cfg(target_os = "windows")]
            {
                let mut names = super::codec_mf::available_encoders();
                // Not `dedup`: that only removes neighbours, and nothing
                // promises the enumeration groups them.
                let mut seen = std::collections::HashSet::new();
                names.retain(|name| seen.insert(name.clone()));
                names
            }
            #[cfg(not(target_os = "windows"))]
            {
                Vec::new()
            }
        })
        .clone()
}

/// Build the H.264 encoder the backend setting asks for.
///
/// Hardware is tried first for `Auto`, because a GPU encoder is the difference
/// between a native-resolution stream that keeps up and one that does not — the
/// software path costs ~24 ms on a 2560x1600 frame against a 16.7 ms budget at
/// 60 fps.
fn create_h264(config: &EncodeConfig) -> Result<Box<dyn VideoEncoder>> {
    #[cfg(target_os = "windows")]
    if config.backend != EncoderBackend::Software {
        // Auto tolerates a per-size or per-session hardware failure by dropping
        // to the CPU; Hardware does not, because the label promises otherwise.
        let allow_fallback = config.backend == EncoderBackend::Auto;
        match super::codec_mf::HardwareH264Encoder::new(config, allow_fallback) {
            Ok(encoder) => {
                info!("H.264 encoding on the GPU via {}", encoder.name());
                return Ok(Box::new(encoder));
            }
            // `Hardware` is a promise the label makes ("fails if unsupported"),
            // so honour it rather than silently doing something slower. `Auto`
            // is the setting that means "whatever works", and it is the default.
            Err(e) if config.backend == EncoderBackend::Hardware => {
                return Err(e.context(
                    "the Hardware encoder backend was requested but is unavailable — \
                     switch to Automatic to allow the software encoder",
                ));
            }
            Err(e) => info!("No hardware encoder ({e}) — using the software encoder"),
        }
    }

    #[cfg(not(target_os = "windows"))]
    if config.backend == EncoderBackend::Hardware {
        anyhow::bail!(
            "hardware encoding is only implemented on Windows — \
             switch to Automatic to allow the software encoder"
        );
    }

    Ok(Box::new(H264VideoEncoder::new(config)?))
}

/// Create an encoder for the given configuration.
///
/// Unimplemented codecs log a warning and fall back. Adding a backend means
/// implementing [`VideoEncoder`] and flipping its [`Availability`].
pub fn create_encoder(config: EncodeConfig) -> Result<Box<dyn VideoEncoder>> {
    let effective = config.effective_codec();
    if config.is_falling_back() {
        warn!(
            "{} is not implemented yet — encoding with {} instead",
            config.codec.label(),
            effective.label()
        );
    }
    // Say so rather than quietly ignoring the request: someone who picked
    // "Hardware" and got the lossless path deserves to know why nothing changed.
    if !effective.is_lossy() && config.backend == EncoderBackend::Hardware {
        warn!(
            "{} has no hardware path — it is a CPU codec by construction",
            effective.label()
        );
    }

    match effective {
        VideoCodec::H264 => create_h264(&config),
        // # No software path, deliberately
        //
        // `effective_codec` only resolves to HEVC on a machine whose GPU can
        // encode it, so reaching here means the hardware is there. There is no
        // CPU HEVC encoder in this build to fall back to and adding one would be
        // a poor trade anyway — software HEVC is several times the cost of
        // software H.264 for a stream that has to stay interactive. A machine
        // that loses its hardware encoder mid-session drops to H.264 through the
        // same rebuild path everything else uses.
        VideoCodec::H265 => {
            let encoder = super::codec_mf::HardwareH264Encoder::new(&config, false)
                .context("H.265 needs a hardware encoder and this machine has none")?;
            info!("H.265 encoding on the GPU via {}", encoder.name());
            Ok(Box::new(encoder))
        }
        VideoCodec::DeltaZstd => Ok(Box::new(DeltaZstdEncoder::new(&config)?)),
        VideoCodec::Raw => Ok(Box::new(RawBgraEncoder::new(&config)?)),
        // Unreachable while effective_codec() resolves everything else, but
        // keeps this exhaustive so adding a backend forces a decision here.
        other => anyhow::bail!("{} encoder is not implemented", other.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(width: u16, height: u16) -> Vec<u8> {
        vec![0x7f; width as usize * height as usize * 4]
    }

    fn raw_config() -> EncodeConfig {
        EncodeConfig { codec: VideoCodec::Raw, ..EncodeConfig::default() }
    }

    #[test]
    fn raw_encoder_passes_pixels_through_unchanged() {
        let mut enc = create_encoder(raw_config()).unwrap();
        let src = bgra(4, 4);
        let out = enc.encode_frame(&src, 4, 4, PixelFormat::Bgra).unwrap();

        assert_eq!(out.data.as_ref(), src.as_slice());
        assert_eq!((out.width, out.height), (4, 4));
        assert!(out.is_keyframe);
        assert_eq!(enc.codec(), VideoCodec::Raw);
    }

    #[test]
    fn sequence_numbers_increment_per_frame() {
        let mut enc = create_encoder(raw_config()).unwrap();
        let src = bgra(2, 2);
        let a = enc.encode_frame(&src, 2, 2, PixelFormat::Bgra).unwrap().sequence_number;
        let b = enc.encode_frame(&src, 2, 2, PixelFormat::Bgra).unwrap().sequence_number;
        assert_eq!(b, a + 1);
    }

    #[test]
    fn undersized_buffer_is_rejected_rather_than_panicking() {
        let mut enc = create_encoder(raw_config()).unwrap();
        let err = enc.encode_frame(&bgra(2, 2), 64, 64, PixelFormat::Bgra).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    fn delta_config() -> EncodeConfig {
        EncodeConfig { codec: VideoCodec::DeltaZstd, ..EncodeConfig::default() }
    }

    #[test]
    fn the_delta_encoder_also_rejects_an_undersized_buffer() {
        let mut enc = create_encoder(delta_config()).unwrap();
        let err = enc.encode_frame(&bgra(2, 2), 64, 64, PixelFormat::Bgra).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    /// Selecting any codec gives you something that encodes.
    ///
    /// 16x16 so H.264's macroblocks have something to work with — and small
    /// enough that a *hardware* encoder will refuse it, which is why the codecs
    /// with no software path are checked for degradation rather than for output.
    /// A real session never streams sixteen pixels; the adaptive controller's
    /// own floor is two orders of magnitude above it.
    #[test]
    fn every_declared_codec_produces_a_working_encoder() {
        for codec in VideoCodec::ALL {
            let cfg = EncodeConfig { codec, ..EncodeConfig::default() };
            let effective = cfg.effective_codec();

            // Two questions, and both have to be yes: the build has the code,
            // and this machine has the hardware. H.265 and AV1 are the ones
            // where they can differ, and AV1 is the one where they usually do.
            if codec.is_available() && codec.is_encodable_here() {
                assert_eq!(effective, codec);
                assert!(!cfg.is_falling_back());
            } else {
                assert_eq!(effective, VideoCodec::H264);
                assert!(cfg.is_falling_back());
            }

            // The GPU-only codecs have no CPU encoder to fall back to, so at a
            // size no GPU will take there is nothing to construct. Everything
            // else must produce a frame.
            let gpu_only = matches!(effective, VideoCodec::H265 | VideoCodec::Av1);
            let Ok(mut enc) = create_encoder(cfg.clone()) else {
                assert!(gpu_only, "{codec:?} must have produced an encoder");
                continue;
            };
            if !gpu_only {
                assert!(
                    enc.encode_frame(&bgra(16, 16), 16, 16, PixelFormat::Bgra).is_ok(),
                    "{codec:?} failed to encode"
                );
            }
            // Whatever happened, the encoder reports what it really produces.
            assert_eq!(enc.codec(), effective);
        }
    }

    #[test]
    fn the_implemented_codecs_are_the_documented_ones() {
        // A reminder to update the docs and UI when this changes.
        let available: Vec<_> = VideoCodec::ALL.into_iter().filter(|c| c.is_available()).collect();
        assert_eq!(
            available,
            vec![
                VideoCodec::DeltaZstd,
                VideoCodec::Raw,
                VideoCodec::H264,
                VideoCodec::H265,
                VideoCodec::Av1,
            ]
        );
    }

    #[test]
    fn unimplemented_codecs_fall_back_to_h264_not_to_raw() {
        // Degrading to raw would mean an unavailable choice silently costs
        // hundreds of MB/s, which is worse than any codec it stood in for.
        for codec in [VideoCodec::Vp9] {
            let cfg = EncodeConfig { codec, ..EncodeConfig::default() };
            assert_eq!(cfg.effective_codec(), VideoCodec::H264);
        }
    }

    /// H.265 falls back on hardware grounds, not on build grounds.
    ///
    /// Whichever way this machine answers, the two must agree: a codec that can
    /// be encoded here runs, and one that cannot degrades to H.264 rather than
    /// to raw or to an error. Written this way because the answer differs
    /// between machines and a fixed expectation would fail on half of them.
    #[test]
    fn h265_runs_where_it_can_and_degrades_to_h264_where_it_cannot() {
        let cfg = EncodeConfig { codec: VideoCodec::H265, ..EncodeConfig::default() };
        let expected = if VideoCodec::H265.is_encodable_here() {
            VideoCodec::H265
        } else {
            VideoCodec::H264
        };
        assert_eq!(cfg.effective_codec(), expected);
        assert_ne!(cfg.effective_codec(), VideoCodec::Raw, "never degrade to raw");
    }

    #[test]
    fn h264_is_the_default_because_lossless_is_too_expensive_to_stream() {
        assert_eq!(VideoCodec::default(), VideoCodec::H264);
        assert_eq!(EncodeConfig::default().effective_codec(), VideoCodec::H264);
    }

    #[test]
    fn only_the_lossy_codecs_are_driven_by_bitrate_and_quality() {
        assert!(VideoCodec::H264.is_lossy());
        assert!(!VideoCodec::DeltaZstd.is_lossy(), "the delta path is pixel-exact");
        assert!(!VideoCodec::Raw.is_lossy());
    }

    #[test]
    fn wire_names_resolve_back_to_their_codec() {
        for codec in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_wire_name(codec.wire_name()), Some(codec));
        }
        assert_eq!(VideoCodec::from_wire_name("nonsense"), None);
    }

    #[test]
    fn only_raw_is_decodable_without_previous_frames() {
        assert!(!VideoCodec::Raw.is_interframe());
        assert!(VideoCodec::DeltaZstd.is_interframe());
    }

    /// Pseudo-random pixels: a flat buffer compresses to nothing by itself,
    /// which would make any keyframe-vs-delta ratio pass trivially.
    fn noisy(width: u16, height: u16) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        (0..width as usize * height as usize * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn the_delta_encoder_actually_compresses_a_static_screen() {
        let mut enc = create_encoder(delta_config()).unwrap();
        assert_eq!(enc.codec(), VideoCodec::DeltaZstd);

        let src = noisy(64, 64);
        let keyframe = enc.encode_frame(&src, 64, 64, PixelFormat::Bgra).unwrap();
        let delta = enc.encode_frame(&src, 64, 64, PixelFormat::Bgra).unwrap();

        assert!(keyframe.is_keyframe);
        assert!(!delta.is_keyframe);
        assert!(
            delta.data.len() < keyframe.data.len() / 4,
            "unchanged frame cost {} bytes vs {} for the keyframe",
            delta.data.len(),
            keyframe.data.len()
        );
    }

    #[test]
    fn wire_names_are_unique_and_lowercase() {
        let mut names: Vec<_> = VideoCodec::ALL.iter().map(|c| c.wire_name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "codec wire names must be unique");
        assert!(VideoCodec::ALL.iter().all(|c| c.wire_name().chars().all(|ch| !ch.is_uppercase())));
    }

    #[test]
    fn raw_is_the_only_uncompressed_codec() {
        assert!(!VideoCodec::Raw.is_compressed());
        assert_eq!(VideoCodec::ALL.iter().filter(|c| c.is_compressed()).count(), 5);
    }

    #[test]
    fn constant_quality_is_the_only_mode_not_driven_by_bitrate() {
        assert!(RateControl::Constant.uses_bitrate());
        assert!(RateControl::Variable.uses_bitrate());
        assert!(!RateControl::ConstantQuality.uses_bitrate());
    }

    #[test]
    fn sanitize_clamps_hand_edited_nonsense() {
        let mut cfg = EncodeConfig {
            fps: 100_000,
            quality: 250,
            keyframe_interval: 999_999,
            target_bitrate_kbps: 5,
            ..EncodeConfig::default()
        };
        cfg.sanitize();

        assert_eq!(cfg.fps, 240);
        assert_eq!(cfg.quality, 100);
        assert_eq!(cfg.keyframe_interval, 3600);
        assert_eq!(cfg.target_bitrate_kbps, 100);
    }

    #[test]
    fn zero_bitrate_means_unlimited_and_survives_sanitize() {
        let mut cfg = EncodeConfig { target_bitrate_kbps: 0, ..EncodeConfig::default() };
        cfg.sanitize();
        assert_eq!(cfg.target_bitrate_kbps, 0);
    }

    #[test]
    fn the_priority_never_lowers_anything_by_itself() {
        // The heart of the setting: it describes what to give up *if* something
        // fails, not what to run at. An earlier version mapped the slider
        // straight onto a resolution, so asking for low latency permanently
        // threw away two thirds of the pixels on a link that was perfectly fine.
        // Nothing here may return a stream setting at all.
        for value in 0..=100u8 {
            let p = LatencyPriority::new(value);
            // These are *floors* the controller may descend to, not targets.
            assert!(p.min_fps() >= 8, "priority {value} would allow a dead stream");
            assert!(!p.summary().is_empty());
        }
    }

    #[test]
    fn the_two_ends_sacrifice_opposite_things() {
        let quality = LatencyPriority::QUALITY;
        let latency = LatencyPriority::LATENCY;

        assert!(!quality.sheds_pixels_first(), "quality-first must keep the pixels");
        assert!(latency.sheds_pixels_first(), "latency-first must keep the frame rate");

        // And each protects what it favours with a floor on that axis.
        assert!(latency.min_fps() > quality.min_fps());
        assert!(
            quality.min_scale().factor() > latency.min_scale().factor(),
            "the quality end must refuse to shrink as far"
        );
        assert_ne!(quality.summary(), latency.summary());
    }

    #[test]
    fn moving_towards_latency_makes_the_controller_quicker_to_react() {
        let steps: Vec<LatencyPriority> = (0..=100).step_by(5).map(LatencyPriority::new).collect();
        for pair in steps.windows(2) {
            let (lower, higher) = (pair[0], pair[1]);
            assert!(
                higher.strain_tolerance() <= lower.strain_tolerance(),
                "reaction got slower moving towards latency at {}",
                higher.value()
            );
            assert!(
                higher.min_fps() >= lower.min_fps(),
                "the frame-rate floor dropped moving towards latency at {}",
                higher.value()
            );
            assert!(
                higher.min_scale().factor() <= lower.min_scale().factor(),
                "the resolution floor rose moving towards latency at {}",
                higher.value()
            );
        }
    }

    #[test]
    fn quality_is_taken_back_more_cautiously_than_it_is_given_up() {
        // Recovering as fast as it degrades makes a marginal link pump visibly
        // between two quality levels, which is worse than staying one step low.
        for value in (0..=100).step_by(5) {
            let p = LatencyPriority::new(value);
            assert!(
                p.recovery_patience() > p.strain_tolerance(),
                "priority {value} would oscillate"
            );
        }
    }

    #[test]
    fn every_position_looks_good_on_a_healthy_link() {
        // The encode quality is the one thing fixed up front, so no position on
        // the slider may set it somewhere that looks bad when nothing is wrong.
        for value in 0..=100u8 {
            let q = LatencyPriority::new(value).encode_quality();
            assert!((60..=85).contains(&q), "priority {value} asks for quality {q}");
        }
        assert!(
            LatencyPriority::LATENCY.encode_quality() < LatencyPriority::QUALITY.encode_quality()
        );
    }

    /// Keyframes are not on this slider, and they are not off either.
    ///
    /// This has been wrong in both directions. It first asserted the latency end
    /// sent them less often than the quality end, encoding the idea that the
    /// interval was a position on the quality trade — it is not. Then it
    /// asserted zero everywhere, encoding the idea that a reliable transport
    /// needs none at all — which ignored that a decoder can lose sync without
    /// anything being lost, and left corruption on screen until the session
    /// ended.
    ///
    /// What is true: every position sends them at the same rare interval,
    /// because this is a repair mechanism rather than a setting.
    #[test]
    fn every_position_repairs_at_the_same_rare_interval() {
        let expected = KEYFRAME_SECONDS * 60;
        for value in 0..=100u8 {
            assert_eq!(LatencyPriority::new(value).keyframe_interval(), expected);
        }
        // Rare enough that the spike is not a rhythm, and often enough that
        // damage does not outlive a coffee.
        assert!((10..=60).contains(&KEYFRAME_SECONDS));
    }

    #[test]
    fn the_slider_is_clamped_and_balanced_by_default() {
        assert_eq!(LatencyPriority::new(200).value(), 100);
        assert_eq!(LatencyPriority::default(), LatencyPriority::BALANCED);
        assert_eq!(LatencyPriority::BALANCED.value(), 50);
    }

    #[test]
    fn every_slider_position_describes_itself() {
        for value in 0..=100u8 {
            assert!(!LatencyPriority::new(value).summary().is_empty());
        }
    }

    #[test]
    fn every_option_is_labelled_and_described() {
        for codec in VideoCodec::ALL {
            assert!(!codec.label().is_empty() && !codec.description().is_empty());
        }
        for backend in EncoderBackend::ALL {
            assert!(!backend.label().is_empty() && !backend.description().is_empty());
        }
        for rc in RateControl::ALL {
            assert!(!rc.label().is_empty() && !rc.description().is_empty());
        }
    }
}
