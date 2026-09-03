//! Global, persisted application settings.
//!
//! Stored as JSON in the platform's config directory. Loading is deliberately
//! forgiving: a missing file yields defaults, and a corrupt one is reported and
//! replaced rather than blocking startup — losing preferences is a far smaller
//! problem than an app that will not open.
//!
//! Capture and encoder settings apply to **new sessions**; a session already
//! running keeps the configuration it started with.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::*;

use tain_core::{
    CaptureConfig, EncodeConfig, EncoderBackend, H264Profile, LatencyPriority, QualityScale,
    RateControl, SessionConfig, VideoCodec,
};

use super::shortcuts::ShortcutBindings;

/// How the guest fits a remote screen that doesn't match its window.
///
/// There is deliberately no "stretch": a distorted desktop is never what anyone
/// wanted, and the two modes here cover the real choice — see the whole screen,
/// or see part of it sharply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum ScalingMode {
    /// Preserve aspect ratio, letterbox the remainder. Never distorts.
    #[default]
    Fit,
    /// One remote pixel per screen pixel. Sharpest; pans when the remote screen
    /// is bigger than the window.
    Actual,
}

impl ScalingMode {
    pub const ALL: [Self; 2] = [Self::Fit, Self::Actual];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Actual => "1:1",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Fit => "Preserve aspect ratio, letterbox the rest",
            Self::Actual => "One remote pixel per screen pixel, panning at the edges",
        }
    }
}

// A removed variant must not cost the user every other preference. Serde's
// derived impl would fail the whole `Settings` struct on an old "Stretch"
// value, and `#[serde(default)]` only covers *missing* fields, not unparseable
// ones — so anything unrecognised lands on Fit instead.
impl<'de> Deserialize<'de> for ScalingMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(match name.as_str() {
            "Actual" => Self::Actual,
            _ => Self::Fit,
        })
    }
}

/// Where the name this machine presents to the other side comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeviceNameMode {
    /// Publish nothing. The other side shows a generated label.
    #[default]
    Anonymous,
    /// The account name you are signed in as.
    SystemUser,
    /// The machine's network name.
    NetworkName,
    /// Whatever is typed into the custom field.
    Custom,
}

impl DeviceNameMode {
    pub const ALL: [Self; 4] =
        [Self::Anonymous, Self::SystemUser, Self::NetworkName, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "Anonymous",
            Self::SystemUser => "System user name",
            Self::NetworkName => "Network PC name",
            Self::Custom => "Custom",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Anonymous => "Publish no name — the other machine shows \"Guest #1\" or similar",
            Self::SystemUser => "The account you are signed in as",
            Self::NetworkName => "This machine's network name",
            Self::Custom => "A name you choose",
        }
    }
}

/// The account name this session is signed in as.
pub fn system_user_name() -> Option<String> {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).ok()
}

/// This machine's network name.
pub fn network_pc_name() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("NAME"))
        .ok()
}

// ── Serializable mirrors of the tain-core enums ───────────────
//
// The core types are deliberately not serde-aware — the wire protocol and the
// preferences file should be free to diverge. These mirrors keep the config
// format stable even if the core enums are reordered.

macro_rules! mirror_enum {
    ($name:ident, $core:ty, { $($variant:ident => $core_variant:ident),+ $(,)? }, $default:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
        pub enum $name {
            #[default]
            $default,
            $($variant),+
        }

        impl $name {
            pub fn to_core(self) -> $core {
                match self {
                    Self::$default => <$core>::$default,
                    $(Self::$variant => <$core>::$core_variant),+
                }
            }

            pub fn from_core(value: $core) -> Self {
                match value {
                    <$core>::$default => Self::$default,
                    $(<$core>::$core_variant => Self::$variant),+
                }
            }
        }
    };
}

mirror_enum!(CodecSetting, VideoCodec, {
    DeltaZstd => DeltaZstd,
    Raw => Raw,
    H265 => H265,
    Av1 => Av1,
    Vp9 => Vp9,
}, H264);

mirror_enum!(BackendSetting, EncoderBackend, {
    Hardware => Hardware,
    Software => Software,
}, Auto);

mirror_enum!(ProfileSetting, H264Profile, {
    Main => Main,
    High => High,
}, Baseline);

mirror_enum!(RateControlSetting, RateControl, {
    Variable => Variable,
    ConstantQuality => ConstantQuality,
}, Constant);

mirror_enum!(ResolutionSetting, QualityScale, {
    High => High,
    Medium => Medium,
    Low => Low,
}, Native);

/// Deserialize one field, falling back to its default rather than failing.
///
/// # Why every enum field needs this
///
/// serde's derived `Deserialize` rejects a variant it does not recognise, and
/// that rejection fails the *whole file* — so one renamed codec or backend
/// resets every unrelated setting the user ever chose. That is what made
/// settings appear to reset on every new build: the file was fine, one field in
/// it had a name this build no longer knew, and `load` fell back wholesale.
///
/// With this, an unreadable field costs that field and nothing else. The
/// hand-written [`ScalingMode`] impl above is the same idea, discovered the same
/// way, for one field at a time.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    // Through `Value` rather than trying `T` directly: a failed deserialize has
    // already consumed the input, so there would be nothing left to fall back
    // with.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// Everything the user can configure globally.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Video
    #[serde(deserialize_with = "lenient")]
    pub codec: CodecSetting,
    #[serde(deserialize_with = "lenient")]
    pub encoder_backend: BackendSetting,
    /// Which GPU to encode on, as part of its name — e.g. "NVIDIA".
    ///
    /// # Why a name fragment and not a chosen device
    ///
    /// There is no device to choose. Media Foundation hands out transforms with
    /// names written by the driver vendor ("Intel(R) Quick Sync Video H.264
    /// Encoder MFT", "AMDh264Encoder"), and those are the only handle on which
    /// card is which. A fragment survives a driver update that rewords the rest.
    ///
    /// Empty means no preference, which is the default and what everyone with
    /// one GPU wants. It matters on a laptop with an integrated *and* a discrete
    /// card, where the system's own order picked the iGPU.
    #[serde(default, deserialize_with = "lenient")]
    pub preferred_encoder: String,
    /// H.264 profile to encode with. See [`H264Profile`].
    #[serde(default, deserialize_with = "lenient")]
    pub h264_profile: ProfileSetting,
    /// Draw decoded frames straight from the GPU, without reading them back.
    ///
    /// # Why this is off by default when it was meant to be always on
    ///
    /// It saves a great deal: the readback is 3 ms a frame on one machine and
    /// 8.8 ms on another — ninety per cent of that machine's decode — and it
    /// sits inside the flow-control loop, because the guest acknowledges a frame
    /// once it has decoded it.
    ///
    /// It was shipped on. On the first machine to try it, a GeForce GT 730 whose
    /// driver reported the extension as supported, the picture froze on the
    /// first frame and the app later crashed. One cause of that is found and
    /// fixed — a registration cache keyed on a raw pointer that survived the
    /// decoder rebuilding its textures, which a resolution change does and that
    /// session did five times. Whether that was the *whole* fault is not known,
    /// and cannot be known from here: the failure is "no picture appears", which
    /// no test can see.
    ///
    /// So it is a choice rather than a default. Turning it on where it works is
    /// a real gain; having it on everywhere while one machine shows a frozen
    /// frame is not a trade worth making for someone who just wants to connect.
    #[serde(default)]
    pub gpu_present: bool,
    /// Whether this machine may be reached with nobody signed in.
    ///
    /// Off by default, and separate from installing the service on purpose.
    /// Installing it buys consent prompts and the lock screen — things that
    /// happen while somebody is already at the machine using it. Answering when
    /// nobody is there is a different proposition with a different risk, and
    /// folding the two into one decision would mean anybody who wanted the
    /// first silently got the second.
    pub unattended_access: bool,
    /// How large this application's own interface is, relative to the DPI
    /// scale the monitor asks for. `1.0` follows the monitor exactly.
    ///
    /// # Why relative rather than absolute
    ///
    /// egui computes `pixels_per_point = zoom_factor * native_pixels_per_point`,
    /// and this is the first of those. Keeping it a multiplier means the
    /// interface still follows a monitor change on its own — someone who wants
    /// it a little larger than Windows suggests gets that on every screen,
    /// rather than pinning one size and having it be wrong on the next one.
    ///
    /// It is also where egui's own Ctrl+plus and Ctrl+minus land, so the two
    /// are kept in step rather than fighting; see `GuestApp::sync_ui_scale`.
    #[serde(default = "one")]
    pub ui_scale: f32,
    #[serde(deserialize_with = "lenient")]
    pub rate_control: RateControlSetting,
    /// What to give up first if the stream cannot keep up, 0-100.
    ///
    /// Not a quality setting — see [`LatencyPriority`]. Every position streams
    /// the full ceiling below when nothing is wrong; this only decides whether
    /// detail or smoothness is sacrificed when something is.
    pub latency_priority: u8,
    /// Frame rate to aim for. The ceiling, not a promise: the adaptive
    /// controller drops below it only when a stage cannot keep up.
    pub target_fps: u32,
    /// How much of the host's display to stream, at most.
    ///
    /// Native by default. This is a deliberate cap for someone on a metered or
    /// slow link who would rather never send full resolution than have the
    /// controller discover that the hard way each session.
    #[serde(deserialize_with = "lenient")]
    pub max_resolution: ResolutionSetting,
    /// 0 means unlimited. Ignored by raw, which cannot throttle.
    pub max_bitrate_kbps: u32,
    /// 0-100, used when rate control is constant-quality.
    pub encode_quality: u8,
    /// Frames between keyframes. 0 means "only when asked".
    pub keyframe_interval: u32,
    pub show_cursor: bool,
    /// Executables to keep out of the streamed audio, e.g. "Discord.exe".
    ///
    /// Several are supported, but not for free: one exclusion is done by the
    /// audio engine itself, and more than one means capturing every *included*
    /// application and summing them. See `audio_capture::start_excluding_many`.
    #[serde(deserialize_with = "lenient")]
    pub exclude_audio_processes: Vec<String>,
    /// The single-app setting this replaced.
    ///
    /// Read, migrated into the list above, and then written back empty. Kept so
    /// an existing config does not quietly lose the app the user chose.
    #[serde(default, deserialize_with = "lenient")]
    pub exclude_audio_process: String,
    ///
    /// # Why a name and not a process id
    ///
    /// A pid is different every launch, so a saved one would exclude whatever
    /// happened to inherit the number — or nothing at all. The name is resolved
    /// to a pid when a session starts, which is also when the app in question is
    /// most likely to be running.
    ///
    /// Empty means exclude nothing, which is the default: sending less than the
    /// user expects is worse than sending all of it.
    /// Opus bitrate in kbit/s. See [`tain_core::AUDIO_BITRATES_KBPS`].
    pub audio_bitrate_kbps: u32,
    /// Let the priority slider pick the encoder's quality target and keyframe
    /// interval, rather than setting them by hand.
    ///
    /// On by default: these two are the encoder settings whose right value
    /// depends on the quality-versus-latency trade, and someone who has not gone
    /// looking for them should get a coherent pair. Resolution and frame rate
    /// are *not* covered — those are ceilings the user states outright, and the
    /// controller adjusts beneath them.
    pub follow_priority: bool,

    // Identity
    /// Where the name shown to the other machine comes from.
    #[serde(deserialize_with = "lenient")]
    pub device_name_mode: DeviceNameMode,
    /// Used when `device_name_mode` is [`DeviceNameMode::Custom`].
    pub custom_device_name: String,

    // Security
    /// How long a guest waits for an answer before being refused.
    pub approval_timeout_secs: u64,
    pub admin_autoreset: bool,
    pub guest_autoreset: bool,
    /// Whether the approval dialog's mouse checkbox starts ticked.
    ///
    /// Off by default. The dialog hands over control of this machine, so the
    /// answer you get by not thinking about it should be the safe one — but
    /// someone who only ever shares with people they trust should not have to
    /// tick two boxes every single time either.
    pub default_allow_mouse: bool,
    /// Whether the approval dialog's keyboard checkbox starts ticked.
    pub default_allow_keyboard: bool,

    // Session
    #[serde(deserialize_with = "lenient")]
    pub scaling_mode: ScalingMode,
    pub fullscreen_on_connect: bool,
    #[serde(deserialize_with = "lenient")]
    pub shortcuts: ShortcutBindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codec: CodecSetting::default(),
            encoder_backend: BackendSetting::default(),
            preferred_encoder: String::new(),
            h264_profile: ProfileSetting::default(),
            gpu_present: false,
            unattended_access: false,
            ui_scale: 1.0,
            rate_control: RateControlSetting::default(),
            latency_priority: LatencyPriority::default().value(),
            // 60, not 30: with the hardware encoder a native-resolution frame
            // costs ~6 ms, so 30 would leave most of the budget unused and the
            // stream feeling half as responsive as the machine can manage. The
            // controller drops it if this particular machine or link cannot.
            target_fps: 60,
            max_resolution: ResolutionSetting::default(),
            max_bitrate_kbps: 0,
            encode_quality: 70,
            keyframe_interval: tain_core::KEYFRAME_SECONDS * 60,
            show_cursor: true,
            exclude_audio_processes: Vec::new(),
            exclude_audio_process: String::new(),
            audio_bitrate_kbps: 160,
            follow_priority: true,
            device_name_mode: DeviceNameMode::default(),
            custom_device_name: String::new(),
            approval_timeout_secs: 30,
            admin_autoreset: false,
            guest_autoreset: true,
            default_allow_mouse: false,
            default_allow_keyboard: false,
            scaling_mode: ScalingMode::default(),
            fullscreen_on_connect: false,
            shortcuts: ShortcutBindings::default(),
        }
    }
}

/// serde's default for [`Settings::ui_scale`], which is not `Default::default`.
fn one() -> f32 {
    1.0
}

/// The range the interface scale may be set to.
///
/// The bottom is where the smallest text is still readable at 100% DPI; the top
/// is where a 1200-point window stops fitting on an ordinary display. Anything
/// outside it is a config file that has been edited by hand or corrupted, and
/// either way an interface nobody can use to put it right.
pub const UI_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=2.5;

impl Settings {
    /// Clamp everything into a sane range.
    ///
    /// Applied after loading as well as after editing, so a hand-edited config
    /// file cannot put the app into a state its own UI would not allow.
    pub fn sanitize(&mut self) {
        // Migrate the single-app setting this replaced. Done here because
        // `sanitize` runs on load, so a config written by an older build keeps
        // the app the user chose instead of silently losing it.
        if !self.exclude_audio_process.is_empty() {
            let name = std::mem::take(&mut self.exclude_audio_process);
            if !self.exclude_audio_processes.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                self.exclude_audio_processes.push(name);
            }
        }
        // Case-insensitive de-duplication: the picker and a hand-edited config
        // can both put the same executable in twice.
        self.exclude_audio_processes.retain(|n| !n.is_empty());
        let mut seen: Vec<String> = Vec::new();
        self.exclude_audio_processes.retain(|n| {
            let lower = n.to_lowercase();
            if seen.contains(&lower) {
                false
            } else {
                seen.push(lower);
                true
            }
        });

        // Trimmed, because it is matched as a substring and a stray space from a
        // paste would silently match nothing at all.
        if self.preferred_encoder.trim().len() != self.preferred_encoder.len() {
            self.preferred_encoder = self.preferred_encoder.trim().to_string();
        }

        // NaN as well as out of range: it arrives from a hand-edited file, and
        // a NaN scale multiplies into a `pixels_per_point` of NaN, which lays
        // the whole interface out at no size at all and cannot be clicked back.
        if !self.ui_scale.is_finite() {
            self.ui_scale = 1.0;
        }
        self.ui_scale = self.ui_scale.clamp(*UI_SCALE_RANGE.start(), *UI_SCALE_RANGE.end());

        self.target_fps = self.target_fps.clamp(1, 240);
        self.approval_timeout_secs = self.approval_timeout_secs.clamp(5, 600);
        self.encode_quality = self.encode_quality.min(100);
        self.keyframe_interval = self.keyframe_interval.min(3600);
        if self.max_bitrate_kbps != 0 {
            self.max_bitrate_kbps = self.max_bitrate_kbps.clamp(100, 500_000);
        }
        if self.custom_device_name.chars().count() > tain_core::MAX_DEVICE_NAME {
            self.custom_device_name =
                self.custom_device_name.chars().take(tain_core::MAX_DEVICE_NAME).collect();
        }
    }

    /// The name this machine publishes to the other side, if any.
    ///
    /// Used in both directions: guests send it with `Auth` so the host's
    /// approval prompt names a machine rather than a number, and hosts send it
    /// in `SessionStart` so the guest's saved list reads the same way.
    pub fn device_name(&self) -> Option<String> {
        let raw = match self.device_name_mode {
            DeviceNameMode::Anonymous => return None,
            DeviceNameMode::SystemUser => system_user_name(),
            DeviceNameMode::NetworkName => network_pc_name(),
            DeviceNameMode::Custom => Some(self.custom_device_name.clone()),
        }?;
        // Same cleaning the receiving end applies, so what is previewed in the
        // settings window is exactly what the other machine will show.
        tain_core::sanitize_device_name(&raw)
    }

    /// Where the slider currently sits.
    pub fn priority(&self) -> LatencyPriority {
        LatencyPriority::new(self.latency_priority)
    }

    /// Which devices the approval dialog offers pre-ticked.
    pub fn default_grants(&self) -> tain_core::ControlGrants {
        tain_core::ControlGrants::new(self.default_allow_mouse, self.default_allow_keyboard)
    }

    /// Frame rate ceiling these settings imply.
    pub fn effective_fps(&self) -> u32 {
        self.target_fps
    }

    /// Resolution ceiling these settings imply.
    pub fn effective_quality(&self) -> QualityScale {
        self.max_resolution.to_core()
    }

    /// Encoder quality target these settings imply.
    pub fn effective_encode_quality(&self) -> u8 {
        if self.follow_priority { self.priority().encode_quality() } else { self.encode_quality }
    }

    /// Keyframe interval these settings imply.
    pub fn effective_keyframe_interval(&self) -> u32 {
        if self.follow_priority {
            self.priority().keyframe_interval()
        } else {
            self.keyframe_interval
        }
    }

    /// Capture configuration these settings imply.
    pub fn to_capture_config(&self) -> CaptureConfig {
        CaptureConfig {
            // Which display a session streams is chosen per session, not in
            // the global settings: two guests can be watching different ones.
            display: None,
            fps: self.effective_fps(),
            quality: self.effective_quality(),
            show_cursor: self.show_cursor,
            native_override: None,
        }
    }

    /// Translate into the session configuration the host server consumes.
    pub fn to_session_config(&self) -> SessionConfig {
        SessionConfig {
            capture: self.to_capture_config(),
            encode: EncodeConfig {
                codec: self.codec.to_core(),
                backend: self.encoder_backend.to_core(),
                profile: self.h264_profile.to_core(),
                preferred_encoder: self.preferred_encoder.clone(),
                rate_control: self.rate_control.to_core(),
                target_bitrate_kbps: self.max_bitrate_kbps,
                quality: self.effective_encode_quality(),
                keyframe_interval: self.effective_keyframe_interval(),
                fps: self.effective_fps(),
            },
            approval_timeout: std::time::Duration::from_secs(self.approval_timeout_secs),
            auto_regen_guest_passcode: self.guest_autoreset,
            exclude_audio_processes: self.exclude_audio_processes.clone(),
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            host_name: self.device_name(),
            priority: self.priority(),
        }
    }

    /// Where settings live. `None` if no config directory can be determined.
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("Tain").join("settings.json"))
    }

    /// Load from disk, falling back to defaults.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            debug!("No config directory — using default settings");
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };

        match serde_json::from_str::<Self>(&text) {
            Ok(mut settings) => {
                settings.sanitize();
                debug!("Loaded settings from {}", path.display());
                settings
            }
            Err(e) => {
                // Never let a bad config file stop the app from starting.
                warn!("Ignoring unreadable settings at {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Persist to disk. Failures are logged, not surfaced — a settings write
    /// failing should not interrupt what the user was doing.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            warn!("Could not create {}: {e}", dir.display());
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    warn!("Could not save settings to {}: {e}", path.display());
                }
            }
            Err(e) => warn!("Could not serialize settings: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_a_roundtrip() {
        let settings = Settings::default();
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn unknown_and_missing_fields_fall_back_to_defaults() {
        // `#[serde(default)]` means a config written by an older or newer build
        // still loads instead of wiping the user's preferences.
        let partial = r#"{ "target_fps": 60, "some_future_field": true }"#;
        let loaded: Settings = serde_json::from_str(partial).unwrap();
        assert_eq!(loaded.target_fps, 60);
        assert_eq!(loaded.codec, CodecSetting::H264);
        assert_eq!(loaded.shortcuts, ShortcutBindings::default());
    }

    /// One unreadable field must cost that field, not the whole file.
    ///
    /// # The bug this is here for
    ///
    /// Settings appeared to reset on every new build. They were written and read
    /// back correctly the whole time — but a derived enum `Deserialize` rejects
    /// an unknown variant, and serde propagates that as a failure of the entire
    /// document, so `load` fell back to defaults and threw away every unrelated
    /// choice the user had made.
    /// A config from the single-app build must keep the app it named.
    #[test]
    fn the_old_single_exclusion_setting_is_migrated_not_dropped() {
        let text = r#"{ "exclude_audio_process": "Discord.exe", "target_fps": 90 }"#;
        let mut loaded: Settings = serde_json::from_str(text).expect("must parse");
        loaded.sanitize();

        assert_eq!(loaded.exclude_audio_processes, vec!["Discord.exe".to_string()]);
        assert!(loaded.exclude_audio_process.is_empty(), "the old field is consumed");
        assert_eq!(loaded.target_fps, 90, "and nothing else is disturbed");
    }

    /// The same executable must not end up excluded twice.
    #[test]
    fn duplicate_exclusions_are_collapsed_regardless_of_case() {
        let mut settings = Settings {
            exclude_audio_processes: vec![
                "Discord.exe".into(),
                "discord.EXE".into(),
                String::new(),
                "steam.exe".into(),
            ],
            exclude_audio_process: "DISCORD.exe".into(),
            ..Settings::default()
        };
        settings.sanitize();
        assert_eq!(
            settings.exclude_audio_processes,
            vec!["Discord.exe".to_string(), "steam.exe".to_string()]
        );
    }

    /// The GPU and profile choices have to survive the trip into a session.
    ///
    /// Both were added for a machine that could not host at all — an Intel
    /// encoder that stalled, on a laptop where the Intel encoder was being
    /// picked over the NVIDIA one. A setting that is stored and then not passed
    /// on is indistinguishable from the bug it was added to fix.
    #[test]
    fn the_gpu_and_profile_choices_reach_the_encoder() {
        let settings = Settings {
            preferred_encoder: "NVIDIA".into(),
            h264_profile: ProfileSetting::Main,
            ..Settings::default()
        };
        let config = settings.to_session_config();
        assert_eq!(config.encode.preferred_encoder, "NVIDIA");
        assert_eq!(config.encode.profile, H264Profile::Main);
    }

    /// Baseline, because it is the only one a guest decoding on its CPU reads.
    ///
    /// `openh264_decodes_every_profile_this_can_produce` in tain-core is what
    /// measures that; this is the half of it that can run anywhere.
    #[test]
    fn the_default_profile_is_the_one_every_guest_can_decode() {
        assert_eq!(Settings::default().h264_profile.to_core(), H264Profile::Baseline);
        assert!(Settings::default().preferred_encoder.is_empty(), "no GPU preference by default");
    }

    #[test]
    fn a_pasted_encoder_name_is_trimmed_so_it_can_match() {
        let mut settings = Settings { preferred_encoder: "  NVIDIA \n".into(), ..Settings::default() };
        settings.sanitize();
        assert_eq!(settings.preferred_encoder, "NVIDIA");
    }

    #[test]
    fn one_unrecognised_enum_variant_does_not_reset_every_other_setting() {
        let text = r#"{
            "codec": "AV1_SomethingThisBuildDoesNotKnow",
            "encoder_backend": "Auto",
            "target_fps": 144,
            "encode_quality": 91,
            "keyframe_interval": 240,
            "show_cursor": false,
            "custom_device_name": "Workshop PC",
            "default_allow_keyboard": false,
            "fullscreen_on_connect": true
        }"#;
        let loaded: Settings = serde_json::from_str(text).expect("must still parse");

        // The bad field falls back on its own.
        assert_eq!(loaded.codec, CodecSetting::default(), "unknown codec resets to default");

        // Everything else survives — these are the settings a user notices losing.
        assert_eq!(loaded.target_fps, 144);
        assert_eq!(loaded.encode_quality, 91);
        assert_eq!(loaded.keyframe_interval, 240);
        assert!(!loaded.show_cursor);
        assert_eq!(loaded.custom_device_name, "Workshop PC");
        assert!(!loaded.default_allow_keyboard);
        assert!(loaded.fullscreen_on_connect);
    }

    /// The same guarantee for every field carrying a name serde can reject.
    #[test]
    fn any_single_unreadable_field_leaves_the_rest_intact() {
        for field in [
            r#""codec": "Nonsense""#,
            r#""encoder_backend": "Nonsense""#,
            r#""rate_control": "Nonsense""#,
            r#""max_resolution": "Nonsense""#,
            r#""device_name_mode": "Nonsense""#,
            r#""scaling_mode": "Nonsense""#,
            r#""shortcuts": "Nonsense""#,
        ] {
            let text = format!("{{ {field}, \"target_fps\": 120, \"encode_quality\": 88 }}");
            let loaded: Settings = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{field} must not fail the file: {e}"));
            assert_eq!(loaded.target_fps, 120, "{field} lost an unrelated setting");
            assert_eq!(loaded.encode_quality, 88, "{field} lost an unrelated setting");
        }
    }

    #[test]
    fn a_settings_file_naming_the_removed_stretch_mode_still_loads() {
        // Regression guard: the derived impl would reject the unknown variant
        // and take every other preference in the file down with it.
        let old = r#"{ "target_fps": 45, "scaling_mode": "Stretch" }"#;
        let loaded: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(loaded.scaling_mode, ScalingMode::Fit);
        assert_eq!(loaded.target_fps, 45, "the rest of the file must survive");

        // And a mode that still exists is preserved.
        let current = r#"{ "scaling_mode": "Actual" }"#;
        assert_eq!(
            serde_json::from_str::<Settings>(current).unwrap().scaling_mode,
            ScalingMode::Actual
        );
    }

    #[test]
    fn anonymous_is_the_default_and_publishes_no_name() {
        let s = Settings::default();
        assert_eq!(s.device_name_mode, DeviceNameMode::Anonymous);
        assert_eq!(s.device_name(), None);
        assert_eq!(s.to_session_config().host_name, None);
    }

    #[test]
    fn a_custom_name_is_published_and_cleaned() {
        let s = Settings {
            device_name_mode: DeviceNameMode::Custom,
            custom_device_name: "  Studio\nPC  ".into(),
            ..Settings::default()
        };
        // Same treatment the receiving end applies, so the preview is honest.
        assert_eq!(s.device_name().as_deref(), Some("StudioPC"));
    }

    #[test]
    fn an_empty_custom_name_falls_back_to_anonymous() {
        // Choosing "Custom" and typing nothing must not publish an empty label.
        let s = Settings {
            device_name_mode: DeviceNameMode::Custom,
            custom_device_name: "   ".into(),
            ..Settings::default()
        };
        assert_eq!(s.device_name(), None);
    }

    #[test]
    fn a_custom_name_is_capped_at_the_protocol_limit() {
        let mut s = Settings {
            device_name_mode: DeviceNameMode::Custom,
            custom_device_name: "n".repeat(500),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.custom_device_name.chars().count(), tain_core::MAX_DEVICE_NAME);
    }

    #[test]
    fn every_name_mode_is_labelled_and_described() {
        for mode in DeviceNameMode::ALL {
            assert!(!mode.label().is_empty());
            assert!(!mode.description().is_empty());
        }
    }

    #[test]
    fn sanitize_clamps_hand_edited_nonsense() {
        let mut s = Settings {
            target_fps: 100_000,
            approval_timeout_secs: 0,
            encode_quality: 250,
            keyframe_interval: 999_999,
            max_bitrate_kbps: 3,
            ..Settings::default()
        };
        s.sanitize();

        assert_eq!(s.target_fps, 240);
        assert_eq!(s.approval_timeout_secs, 5);
        assert_eq!(s.encode_quality, 100);
        assert_eq!(s.keyframe_interval, 3600);
        assert_eq!(s.max_bitrate_kbps, 100);
    }

    #[test]
    fn zero_bitrate_means_unlimited_and_is_not_clamped_up() {
        let mut s = Settings { max_bitrate_kbps: 0, ..Settings::default() };
        s.sanitize();
        assert_eq!(s.max_bitrate_kbps, 0);
    }

    /// A scale that is not a number lays the interface out at no size at all.
    ///
    /// It cannot come from the slider, only from a hand-edited or corrupted
    /// file — and the result is a window with nothing clickable in it, which is
    /// also a window the setting cannot be put right from.
    #[test]
    fn a_scale_that_is_not_a_number_becomes_one() {
        for broken in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut s = Settings { ui_scale: broken, ..Settings::default() };
            s.sanitize();
            assert_eq!(s.ui_scale, 1.0, "{broken} should have become 1.0");
        }
    }

    #[test]
    fn the_interface_scale_stays_inside_a_usable_range() {
        let mut tiny = Settings { ui_scale: 0.01, ..Settings::default() };
        tiny.sanitize();
        assert_eq!(tiny.ui_scale, *UI_SCALE_RANGE.start());

        let mut huge = Settings { ui_scale: 40.0, ..Settings::default() };
        huge.sanitize();
        assert_eq!(huge.ui_scale, *UI_SCALE_RANGE.end());

        // And an ordinary choice is left exactly alone.
        let mut ordinary = Settings { ui_scale: 1.25, ..Settings::default() };
        ordinary.sanitize();
        assert_eq!(ordinary.ui_scale, 1.25);
    }

    /// Following the monitor is the default, and a config from before this
    /// setting existed has to keep doing that.
    #[test]
    fn a_config_without_a_scale_follows_the_monitor() {
        let settings: Settings =
            serde_json::from_str("{}").expect("every field should have a default");
        assert_eq!(settings.ui_scale, 1.0);
        assert_eq!(Settings::default().ui_scale, 1.0);
    }

    #[test]
    fn every_mirrored_enum_round_trips_through_core() {
        for codec in VideoCodec::ALL {
            assert_eq!(CodecSetting::from_core(codec).to_core(), codec);
        }
        for backend in EncoderBackend::ALL {
            assert_eq!(BackendSetting::from_core(backend).to_core(), backend);
        }
        for rc in RateControl::ALL {
            assert_eq!(RateControlSetting::from_core(rc).to_core(), rc);
        }
        for profile in H264Profile::ALL {
            assert_eq!(ProfileSetting::from_core(profile).to_core(), profile);
        }
        for scale in QualityScale::ALL {
            assert_eq!(ResolutionSetting::from_core(scale).to_core(), scale);
        }
    }

    #[test]
    fn capture_follows_the_host_display_not_a_fixed_size() {
        let capture = Settings::default().to_capture_config();
        assert!(capture.native_override.is_none(), "must query the real display");

        // Default is the full display, rounded down to even for chroma
        // subsampling — so an odd-sized display legitimately loses a row.
        let (nw, nh) = capture.native_size();
        let (sw, sh) = capture.dimensions();
        assert!(nw - sw <= 1 && nh - sh <= 1, "native {nw}x{nh} streamed as {sw}x{sh}");
        assert_eq!(sw % 2, 0);
        assert_eq!(sh % 2, 0);
    }

    #[test]
    fn no_position_on_the_slider_lowers_the_stream_by_itself() {
        // The correction this whole design turns on: the priority says what to
        // give up *if* something struggles. It must not pre-emptively give
        // anything up on a machine and a link that are coping fine.
        let full = Settings::default().to_capture_config().dimensions();
        for value in [0u8, 25, 50, 75, 100] {
            let s = Settings { latency_priority: value, ..Settings::default() };
            let config = s.to_session_config();
            assert_eq!(
                config.capture.dimensions(),
                full,
                "priority {value} threw away pixels before anything went wrong"
            );
            assert_eq!(config.capture.fps, 60, "priority {value} lowered the frame rate");
            assert_eq!(config.priority.value(), value, "the priority must reach the session");
        }
    }

    #[test]
    fn the_resolution_ceiling_is_the_users_to_set() {
        // Someone on a metered link should be able to say "never full
        // resolution" outright rather than waiting for the controller to
        // discover it every session.
        let capped = Settings {
            max_resolution: ResolutionSetting::Medium,
            ..Settings::default()
        };
        let full = Settings::default().to_capture_config().dimensions();
        assert!(capped.to_capture_config().dimensions().0 < full.0);
        assert_eq!(capped.effective_quality(), QualityScale::Medium);
    }

    #[test]
    fn the_slider_drives_the_encoder_settings_it_claims_to() {
        let quality = Settings {
            latency_priority: LatencyPriority::QUALITY.value(),
            ..Settings::default()
        };
        let latency = Settings {
            latency_priority: LatencyPriority::LATENCY.value(),
            ..Settings::default()
        };

        let (q, l) = (quality.to_session_config(), latency.to_session_config());
        assert!(l.encode.quality < q.encode.quality, "coarser quantisation");
        // Keyframes are not on this slider. They are a repair mechanism, not a
        // position on the quality trade, so both ends get the same rare interval.
        assert_eq!(l.encode.keyframe_interval, q.encode.keyframe_interval);
        assert!(l.encode.keyframe_interval > 0, "corruption must eventually be repaired");
        assert_eq!(l.encode.fps, l.capture.fps, "encoder and capture must agree on rate");
        // And neither end lowers the ceiling.
        assert_eq!(l.capture.dimensions(), q.capture.dimensions());
    }

    #[test]
    fn turning_off_follow_priority_hands_back_the_encoder_controls() {
        let manual = Settings {
            follow_priority: false,
            latency_priority: LatencyPriority::LATENCY.value(),
            target_fps: 24,
            encode_quality: 91,
            keyframe_interval: 15,
            ..Settings::default()
        };
        assert_eq!(manual.effective_encode_quality(), 91);
        assert_eq!(manual.effective_keyframe_interval(), 15);

        // Frame rate and resolution are ceilings the user always states
        // outright, so the toggle has no bearing on them either way.
        assert_eq!(manual.effective_fps(), 24);
        assert_eq!(manual.effective_quality(), QualityScale::Native);
    }

    #[test]
    fn session_config_reflects_the_settings() {
        // follow_priority off, so the individual values below are the ones that
        // reach the server rather than the slider's.
        let s = Settings {
            follow_priority: false,
            target_fps: 60,
            codec: CodecSetting::H265,
            encoder_backend: BackendSetting::Hardware,
            rate_control: RateControlSetting::Variable,
            max_bitrate_kbps: 12_000,
            keyframe_interval: 60,
            show_cursor: false,
            approval_timeout_secs: 45,
            guest_autoreset: false,
            ..Settings::default()
        };
        let cfg = s.to_session_config();

        assert_eq!(cfg.capture.fps, 60);
        assert!(!cfg.capture.show_cursor);
        assert_eq!(cfg.encode.codec, VideoCodec::H265);
        assert_eq!(cfg.encode.backend, EncoderBackend::Hardware);
        assert_eq!(cfg.encode.rate_control, RateControl::Variable);
        assert_eq!(cfg.encode.target_bitrate_kbps, 12_000);
        assert_eq!(cfg.encode.keyframe_interval, 60);
        assert_eq!(cfg.approval_timeout.as_secs(), 45);
        assert!(!cfg.auto_regen_guest_passcode);
    }

    /// An unimplemented codec degrades to H.264, never to raw.
    ///
    /// H.265 is deliberately not in this list any more: it is implemented, so
    /// whether it falls back depends on whether *this machine* has an HEVC
    /// encoder — a hardware question, not a build one. Asserting it always falls
    /// back would fail on every machine that can actually do it.
    #[test]
    fn selecting_an_unimplemented_codec_falls_back_to_h264_not_raw() {
        // Degrading to raw would silently cost hundreds of MB/s, which is worse
        // than whichever codec the user actually asked for.
        let s = Settings { codec: CodecSetting::Vp9, ..Settings::default() };
        let encode = s.to_session_config().encode;
        assert!(encode.is_falling_back());
        assert_eq!(encode.effective_codec(), VideoCodec::H264);
    }

    #[test]
    fn h264_is_what_a_fresh_install_streams_with() {
        let s = Settings::default();
        let encode = s.to_session_config().encode;
        assert!(!encode.is_falling_back());
        assert_eq!(encode.effective_codec(), VideoCodec::H264);
    }

    #[test]
    fn every_scaling_mode_is_labelled_and_described() {
        for mode in ScalingMode::ALL {
            assert!(!mode.label().is_empty());
            assert!(!mode.description().is_empty());
        }
        assert_eq!(ScalingMode::default(), ScalingMode::Fit);
    }
}
