//! tain-core: P2P remote desktop protocol, networking, and video pipeline.
//!
//! This crate provides the building blocks for a remote desktop application
//! using [iroh](https://iroh.computer) for P2P QUIC connections with automatic
//! NAT traversal (hole punching + relay fallback).
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────┐  iroh P2P QUIC (ALPN: "tain/1")   ┌──────────┐
//! │  Host    │ ◄════════════════════════════► │  Guest   │
//! │          │                                  │          │
//! │ Screen   │  Frame stream (H.264/raw BGRA)   │ Decoder  │
//! │ Capture  │ ──────────────────────────────► │ Display  │
//! │ Encode   │                                  │          │
//! │          │  Input stream (mouse/keyboard)   │ Key/Mouse│
//! │ Input    │ ◄═════════════════════════════  │ Events   │
//! │ Inject   │                                  │          │
//! └──────────┘                                  └──────────┘
//! ```
//!
//! ## Passcode System
//!
//! - **Master passcodes** grant immediate keyboard/mouse control (no approval needed)
//! - **Guest passcodes** stream **nothing** until the host approves the connection.
//!   Approval is per connection, so reconnecting with the same code asks again;
//!   multiple guests can be connected at once, each independently approved.
//! - Passcodes are encoded as human-friendly strings derived from random 32-byte secrets

pub mod protocol;
pub mod connection_code;
pub mod network;
pub mod passcode;
pub mod screen_capture;
pub mod cursor;
pub mod preview;
pub mod wallpaper;
#[cfg(target_os = "windows")]
pub mod audio_capture;
#[cfg(target_os = "windows")]
pub mod capture_dxgi;
#[cfg(target_os = "windows")]
pub mod capture_nv12;
#[cfg(target_os = "windows")]
pub mod capture_shared;
pub mod encoder;
pub mod adaptive;
pub mod codec_delta;
pub mod codec_h264;
pub mod h264_bitstream;
#[cfg(target_os = "windows")]
pub mod codec_mf;
#[cfg(target_os = "windows")]
pub mod codec_mf_decode;
#[cfg(target_os = "windows")]
pub mod decode_rgba;
// Gated on the build script finding an ffmpeg tree; see tain-core/build.rs.
#[cfg(all(target_os = "windows", db_ffmpeg))]
pub mod ffmpeg_runtime;
#[cfg(all(target_os = "windows", db_ffmpeg))]
pub mod decode_ffmpeg;
#[cfg(all(target_os = "windows", db_ffmpeg))]
pub mod audio_codec;
#[cfg(all(target_os = "windows", db_ffmpeg))]
pub mod audio_render;
#[cfg(all(target_os = "windows", db_ffmpeg))]
pub mod audio_output;
pub mod agent_pipe;
pub mod capture_follow;
pub mod input_inject;
pub mod pipe_security;
pub mod service_control;
pub mod session;

/// Get the ffmpeg runtime ready, off the startup path.
///
/// # Why at startup rather than on first use
///
/// Two reasons, both learned from a real two-machine test. The first session
/// otherwise pays for extracting the embedded libraries — a visible stall right
/// when the user is watching. And the log said nothing about hardware decode
/// until a session had already started and gone wrong, which is exactly when it
/// is least useful.
///
/// On a background thread because it writes tens of megabytes on first run and
/// the window should open regardless.
pub fn warm_up_ffmpeg() {
    #[cfg(all(target_os = "windows", db_ffmpeg))]
    std::thread::spawn(|| {
        let started = std::time::Instant::now();
        if ffmpeg_runtime::available() {
            tracing::info!(
                "Hardware decode and audio ready ({} ms)",
                started.elapsed().as_millis()
            );
        }
        // The unavailable case logs its own reason inside `available`.
    });
}

/// Applications currently playing audio, for an exclusion picker.
///
/// Empty on platforms and builds without per-process capture, which is the
/// right answer there: nothing can be excluded, so nothing is offered.
pub fn playing_audio() -> Vec<AudioSession> {
    #[cfg(target_os = "windows")]
    {
        return audio_capture::audio_sessions().into_iter().map(Into::into).collect();
    }
    #[allow(unreachable_code)]
    Vec::new()
}

/// One application with an audio session.
///
/// Mirrored here rather than re-exported so the UI compiles on every platform
/// and in builds without ffmpeg, where the audio modules do not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSession {
    pub pid: u32,
    pub name: String,
}

#[cfg(target_os = "windows")]
impl From<audio_capture::AudioSession> for AudioSession {
    fn from(session: audio_capture::AudioSession) -> Self {
        Self { pid: session.pid, name: session.name }
    }
}

/// Opus bitrates offered in the settings, in kbit/s.
///
/// Stated here rather than in `audio_codec` because the UI has to draw them in
/// builds without ffmpeg, where that module is not compiled at all.
pub const AUDIO_BITRATES_KBPS: [u32; 3] = [96, 160, 256];

// Re-exports for convenience.
pub use iroh::Endpoint;
pub use iroh::endpoint::presets;
pub use connection_code::ConnectionCode;
pub use adaptive::{
    AdaptiveController, Bottleneck, BottleneckSet, GuestTelemetry, Observation, StreamPlan,
};
pub use encoder::{
    Availability, EncodeConfig, EncoderBackend, H264Profile, LatencyPriority, RateControl,
    VideoCodec, KEYFRAME_SECONDS,
};
pub use network::{
    ApprovalDecision, ApprovalRequest, GuestClient, HostServer, SessionConfig, ALPN,
};
pub use passcode::{PasscodeStore, PasscodeType};
pub use protocol::{
    sanitize_device_name, ControlGrants, ControlStateMessage, FrameMessage, InputDevice,
    InputEvent, Message, MAX_DEVICE_NAME, MAX_MESSAGE_SIZE,
};
pub use screen_capture::{CaptureConfig, PixelFormat, QualityScale};
pub use session::{SessionInfo, SessionManager};
