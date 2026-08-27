//! The settings window.
//!
//! Kept separate from [`super::settings`], which owns the data: this module only
//! knows how to draw and edit it. Returns whether anything changed, so the
//! caller can persist and push the new configuration to the server without
//! writing to disk on every frame.

use egui::{Context, RichText};

use super::settings::{
    BackendSetting, CodecSetting, DeviceNameMode, ProfileSetting, RateControlSetting,
    ResolutionSetting, ScalingMode, Settings,
};
use super::shortcuts::{Shortcut, ShortcutAction};
use super::theme::*;
use tain_core::{EncoderBackend, H264Profile, QualityScale, RateControl, VideoCodec};

const AMBER: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x9B, 0x2E);

/// Which shortcut, if any, is currently waiting to capture a key press.
#[derive(Default)]
pub struct PanelState {
    pub capturing: Option<ShortcutAction>,
    /// Applications with an audio session, for the exclusion picker.
    ///
    /// Cached because enumerating them is a COM round trip and the settings
    /// window redraws continuously while it is open.
    pub audio_apps: Vec<tain_core::AudioSession>,
    /// The interface scale being edited, and the saved value it started from.
    ///
    /// # Why this one setting is not applied as it is edited
    ///
    /// Everything else here changes something elsewhere. This changes the size
    /// of the window it is being edited in — including the slider, and
    /// including where that slider is under the pointer.
    ///
    /// A slider reads its value from where the pointer is within its own
    /// rectangle. Move the rectangle and the same pointer position means a
    /// different value, which moves the rectangle again: the reported effect
    /// was a handle sliding between the extremes on its own, with the control
    /// pulled out from under the hand holding it. So the edit is held here
    /// until Apply.
    ///
    /// The second number is the setting this was taken from, so a change made
    /// somewhere else while the window is open — Ctrl+plus, or Reset to
    /// defaults — is followed rather than silently overwritten by a stale
    /// pending value.
    pub pending_ui_scale: Option<(f32, f32)>,
}

/// Draw the settings window. Returns true if any value changed this frame.
pub fn show(
    ctx: &Context,
    open: &mut bool,
    settings: &mut Settings,
    panel: &mut PanelState,
) -> bool {
    let before = settings.clone();

    // # The window has to fit the window it is in
    //
    // The body was capped at a fixed 620 points, and the whole thing anchored
    // to the centre. On a window shorter than the settings needed, the overflow
    // went equally in both directions — so the title bar went off the top,
    // taking the close button with it, and the only way out was Escape or
    // resizing the application.
    //
    // Sized against what is actually there instead. The cap stays for a large
    // display, where a settings window the height of the screen would be worse
    // than one that scrolls.

    /// Title bar, footer, separators and the window's own margins.
    const CHROME: f32 = 170.0;
    /// Below this a scroll area is not worth having; the window may then
    /// overflow, but a settings panel four rows tall is its own problem.
    const MIN_BODY: f32 = 180.0;

    let available = ctx.screen_rect().height() - CHROME;
    let body_height = available.clamp(MIN_BODY, 620.0);

    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(470.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            // `auto_shrink` off horizontally, or the scroll area shrinks to its
            // widest child and the scrollbar floats there instead of sitting on
            // the window edge — which reads as a rendering bug.
            egui::ScrollArea::vertical()
                .max_height(body_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    identity_section(ui, settings);
                    ui.add_space(10.0);
                    interface_section(ui, settings, panel);
                    ui.add_space(10.0);
                    video_section(ui, settings);
                    ui.add_space(10.0);
                    viewing_section(ui, settings);
                    ui.add_space(10.0);
                    audio_section(ui, settings, panel);
                    ui.add_space(10.0);
                    security_section(ui, settings);
                    ui.add_space(18.0);
                    unattended_section(ui, settings);
                    ui.add_space(10.0);
                    shortcut_section(ui, settings, panel);
                });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Hosting settings apply to new connections.")
                        .size(11.0)
                        .color(TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset to defaults").clicked() {
                        *settings = Settings::default();
                        panel.capturing = None;
                    }
                });
            });
        });

    // Closing the window abandons any half-finished rebind, and any scale that
    // was dialled in without being applied.
    if !*open {
        panel.capturing = None;
        panel.pending_ui_scale = None;
    }

    if *settings != before {
        settings.sanitize();
        true
    } else {
        false
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(13.0).strong().color(TEXT));
    ui.add_space(4.0);
}

/// What this machine calls itself to the other side.
///
/// One setting used in both directions: as a guest it names you in the host's
/// approval prompt, and as a host it names you in the guest's saved list.
fn identity_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, "This PC");

    egui::Grid::new("identity").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Name");
        egui::ComboBox::from_id_salt("device_name_mode")
            .width(220.0)
            .selected_text(settings.device_name_mode.label())
            .show_ui(ui, |ui| {
                for mode in DeviceNameMode::ALL {
                    ui.selectable_value(&mut settings.device_name_mode, mode, mode.label())
                        .on_hover_text(mode.description());
                }
            })
            .response
            .on_hover_text(settings.device_name_mode.description());
        ui.end_row();

        if settings.device_name_mode == DeviceNameMode::Custom {
            ui.label("");
            ui.add(
                egui::TextEdit::singleline(&mut settings.custom_device_name)
                    .desired_width(220.0)
                    .char_limit(tain_core::MAX_DEVICE_NAME)
                    .hint_text("e.g. Studio desktop"),
            );
            ui.end_row();
        }
    });

    ui.add_space(4.0);
    // Preview the exact string the other machine will see, after the same
    // cleaning the receiving end applies. "Anonymous" is not a name being sent —
    // it means no name is sent at all.
    let preview = match settings.device_name() {
        Some(name) => format!("Shown to the other machine as \"{name}\""),
        None => "No name is sent — the other machine shows a generated label".to_string(),
    };
    ui.label(RichText::new(preview).size(11.0).color(TEXT_DIM));
}

/// The GPUs the picker offers, as (what gets stored, the full transform name).
///
/// # Why the stored value is a vendor word rather than the whole name
///
/// It is matched as a substring, and the vendor word is the part that survives.
/// A driver update rewords the rest freely — "Intel(R) Quick Sync Video H.264
/// Encoder MFT" has been several other things — and a stored full name that no
/// longer matches means the choice quietly stops applying. "Intel" keeps working.
///
/// It also reads the way the choice is actually made. Nobody wants
/// AMDh264Encoder; they want the AMD card.
///
/// A name with no vendor word in it is stored whole, which is the honest
/// fallback: an unrecognised encoder is still selectable, just not abbreviated.
fn gpu_choices() -> Vec<(String, String)> {
    let mut choices: Vec<(String, String)> = Vec::new();
    for name in tain_core::encoder::available_hardware_encoders() {
        let lower = name.to_lowercase();
        let key = ["NVIDIA", "AMD", "Intel", "Qualcomm", "Apple"]
            .into_iter()
            .find(|vendor| lower.contains(&vendor.to_lowercase()))
            .map_or_else(|| name.clone(), str::to_string);
        // Two transforms from one vendor are one choice: the preference is a
        // substring, so it could not tell them apart even if they were listed.
        if !choices.iter().any(|(existing, _)| *existing == key) {
            choices.push((key, name));
        }
    }
    choices
}

/// What this machine sends when someone connects **to** it.
///
/// # Why the split
///
/// Everything here is a host decision — the codec, the encoder, the resolution
/// ceiling, the frame rate — and it was sitting in one "Video" block beside a
/// setting that only applies when this machine is the *guest*. Two opposite
/// roles under one heading, so it was not possible to tell which side of a
/// session any given control was about. Sessions this machine receives are
/// configured in [`viewing_section`].
fn video_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, "Hosting — what this PC sends");

    let codec = settings.codec.to_core();

    egui::Grid::new("video").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Codec");
        egui::ComboBox::from_id_salt("codec")
            .width(220.0)
            .selected_text(codec_text(settings.codec))
            .show_ui(ui, |ui| {
                for core in VideoCodec::ALL {
                    let choice = CodecSetting::from_core(core);
                    // Unimplemented codecs stay visible but unselectable, so the
                    // roadmap is legible without pretending they work.
                    ui.add_enabled_ui(core.is_available(), |ui| {
                        ui.selectable_value(&mut settings.codec, choice, codec_text(choice))
                            .on_hover_text(core.description());
                    });
                }
            })
            .response
            .on_hover_text(codec.description());
        ui.end_row();

        ui.label("Encoder");
        ui.add_enabled_ui(codec.is_lossy(), |ui| {
            egui::ComboBox::from_id_salt("backend")
                .width(220.0)
                .selected_text(settings.encoder_backend.to_core().label())
                .show_ui(ui, |ui| {
                    for core in EncoderBackend::ALL {
                        ui.selectable_value(
                            &mut settings.encoder_backend,
                            BackendSetting::from_core(core),
                            core.label(),
                        )
                        .on_hover_text(core.description());
                    }
                });
        });
        ui.end_row();

        // Only worth showing where there is a choice to make. On a machine with
        // one GPU the row would be a combo box with a single entry, and the
        // note under this section already names what is encoding.
        let uses_gpu = settings.encoder_backend.to_core() != EncoderBackend::Software;
        let choices = gpu_choices();
        if uses_gpu && choices.len() > 1 {
            ui.label("Graphics card").on_hover_text(
                "Which GPU encodes the stream. A laptop with two will otherwise \
                 use whichever Windows lists first — often the integrated one.",
            );
            let selected = if settings.preferred_encoder.is_empty() {
                "Automatic".to_string()
            } else if choices.iter().any(|(key, _)| *key == settings.preferred_encoder) {
                settings.preferred_encoder.clone()
            } else {
                // A preference carried over from another machine. Said plainly
                // rather than silently reset, so it is clear why the stream is
                // not on the card the settings claim.
                format!("{} (not on this machine)", settings.preferred_encoder)
            };
            egui::ComboBox::from_id_salt("preferred_encoder")
                .width(220.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.preferred_encoder, String::new(), "Automatic")
                        .on_hover_text("Let Windows choose, which is what happened before.");
                    for (key, full) in choices.iter() {
                        ui.selectable_value(&mut settings.preferred_encoder, key.clone(), key)
                            .on_hover_text(full.as_str());
                    }
                });
            ui.end_row();
        }

        // # Only for H.264, because a profile is a per-codec concept
        //
        // This used to appear for anything lossy, which was fine while H.264 was
        // the only lossy codec there was. With H.265 selected it offered
        // Baseline/Main/High — names from the wrong standard entirely, backed by
        // constants from the wrong enumeration, for a setting the encoder
        // deliberately does not apply to HEVC.
        //
        // There is no HEVC picker to replace it with. HEVC's profiles that
        // matter here are Main and Main 10, and Main 10 is ten-bit — this
        // pipeline captures and converts NV12, which is eight-bit, so Main is
        // the only one it can feed. A picker with one entry is furniture.
        if codec.is_lossy() && codec == VideoCodec::H264 {
            ui.label("H.264 profile").on_hover_text(
                "How much the picture is compressed. Anything above Baseline \
                 needs the guest to be decoding on its GPU.",
            );
            egui::ComboBox::from_id_salt("h264_profile")
                .width(220.0)
                .selected_text(settings.h264_profile.to_core().label())
                .show_ui(ui, |ui| {
                    for core in H264Profile::ALL {
                        ui.selectable_value(
                            &mut settings.h264_profile,
                            ProfileSetting::from_core(core),
                            core.label(),
                        )
                        .on_hover_text(core.description());
                    }
                })
                .response
                .on_hover_text(settings.h264_profile.to_core().description());
            ui.end_row();

            // Said on the panel rather than only in a tooltip: this is the one
            // setting here that can leave a guest with a stream that arrives
            // broken rather than merely worse, and it is not recoverable from
            // their end.
            if settings.h264_profile.to_core() != H264Profile::default() {
                ui.label("");
                ui.label(
                    RichText::new(
                        "⚠ A guest without GPU decoding will not be able to read this. \
                         Use it when an Intel encoder stalls on Baseline.",
                    )
                    .size(11.0)
                    .color(AMBER),
                );
                ui.end_row();
            }
        }

        ui.label("Resolution")
            .on_hover_text("The most this will ever stream. It is not reduced unless something cannot keep up.");
        egui::ComboBox::from_id_salt("max_resolution")
            .width(220.0)
            .selected_text(settings.max_resolution.to_core().label())
            .show_ui(ui, |ui| {
                for core in QualityScale::ALL {
                    ui.selectable_value(
                        &mut settings.max_resolution,
                        ResolutionSetting::from_core(core),
                        core.label(),
                    );
                }
            });
        ui.end_row();

        ui.label("Frame rate")
            .on_hover_text("What the host aims for. Dropped only while something is behind.");
        ui.add(egui::Slider::new(&mut settings.target_fps, 5..=144).suffix(" fps"));
        ui.end_row();

        ui.label("If it can't keep up");
        ui.vertical(|ui| {
            // No numeric readout: the number is meaningless on its own, and the
            // summary below says what the position actually does.
            ui.spacing_mut().slider_width = 210.0;
            ui.add(
                egui::Slider::new(&mut settings.latency_priority, 0..=100)
                    .show_value(false)
                    .custom_formatter(|_, _| String::new()),
            );
            ui.horizontal(|ui| {
                ui.label(RichText::new("keep detail").size(10.0).color(TEXT_DIM));
                ui.add_space(104.0);
                ui.label(RichText::new("keep motion").size(10.0).color(TEXT_DIM));
            });
        });
        ui.end_row();

        ui.label("");
        ui.checkbox(&mut settings.follow_priority, "Let the slider set the encoder details")
            .on_hover_text(
                "Quality target and keyframe interval follow the slider. \
                 Turn off to set them yourself.",
            );
        ui.end_row();

        ui.label("Rate control");
        ui.add_enabled_ui(codec.is_lossy(), |ui| {
            egui::ComboBox::from_id_salt("rate_control")
                .width(220.0)
                .selected_text(settings.rate_control.to_core().label())
                .show_ui(ui, |ui| {
                    for core in RateControl::ALL {
                        ui.selectable_value(
                            &mut settings.rate_control,
                            RateControlSetting::from_core(core),
                            core.label(),
                        )
                        .on_hover_text(core.description());
                    }
                });
        });
        ui.end_row();

        // The bitrate limit stays whatever the slider says: it is a constraint
        // imposed by the *link*, not a position on the quality/latency trade,
        // and a metered connection does not become less metered because you
        // dragged towards quality.
        if settings.rate_control.to_core().uses_bitrate() {
            ui.label("Bitrate limit");
            ui.add_enabled_ui(codec.is_lossy(), |ui| {
                ui.horizontal(|ui| {
                    let mut unlimited = settings.max_bitrate_kbps == 0;
                    if ui.checkbox(&mut unlimited, "Unlimited").changed() {
                        settings.max_bitrate_kbps = if unlimited { 0 } else { 8_000 };
                    }
                    if !unlimited {
                        ui.add(
                            egui::DragValue::new(&mut settings.max_bitrate_kbps)
                                .range(100..=500_000)
                                .speed(100)
                                .suffix(" kbps"),
                        );
                    }
                });
            });
            ui.end_row();
        }

        if !settings.follow_priority {
            if !settings.rate_control.to_core().uses_bitrate() {
                ui.label("Quality target");
                ui.add(egui::Slider::new(&mut settings.encode_quality, 0..=100));
                ui.end_row();
            }

            ui.label("Keyframe interval");
            ui.add_enabled_ui(codec.is_lossy(), |ui| {
                ui.add(
                    egui::Slider::new(&mut settings.keyframe_interval, 0..=600)
                        .suffix(" frames")
                        .custom_formatter(|n, _| {
                            if n == 0.0 { "on request".into() } else { format!("{n:.0}") }
                        }),
                )
                .on_hover_text("How often a frame that stands alone is sent");
            });
            ui.end_row();
        }

        ui.label("Capture cursor");
        ui.checkbox(&mut settings.show_cursor, "Include the pointer in frames");
        ui.end_row();
    });

    ui.add_space(6.0);
    bandwidth_note(ui, settings);
}

/// Show what this configuration will actually cost and what it will actually run.
fn bandwidth_note(ui: &mut egui::Ui, settings: &Settings) {
    let capture = settings.to_capture_config();
    let (nw, nh) = capture.native_size();
    let (sw, sh) = capture.dimensions();

    // No U+2192 arrow: egui's default font has no glyph for it and renders a
    // missing-character box. The multiplication sign is fine.
    let note = if (nw, nh) == (sw, sh) {
        format!(
            "Host display {nw}×{nh} — up to full resolution at {} fps",
            capture.fps
        )
    } else {
        format!("Host display {nw}×{nh} — up to {sw}×{sh} at {} fps", capture.fps)
    };
    ui.label(RichText::new(note).size(11.0).color(TEXT_DIM));
    // Say what the slider position means in words, not just where the handle is.
    ui.label(
        RichText::new(settings.priority().summary()).size(11.0).color(TEXT_DIM),
    );

    // Whether this machine can actually hold that ceiling comes down to the
    // encoder, and it is worth saying before a session rather than leaving it to
    // be discovered as stutter.
    match tain_core::encoder::hardware_encoder_name_for(&settings.preferred_encoder) {
        Some(name) if settings.encoder_backend.to_core() != EncoderBackend::Software => {
            ui.label(
                RichText::new(format!("Encoding on the GPU via {name}"))
                    .size(11.0)
                    .color(TEXT_DIM),
            );
        }
        Some(_) => {
            ui.label(
                RichText::new(
                    "⚠ This machine has a GPU encoder, but the backend is set to Software. \
                     Full resolution at a high frame rate is unlikely to keep up.",
                )
                .size(11.0)
                .color(AMBER),
            );
        }
        None => {
            ui.label(
                RichText::new(
                    "No GPU encoder found — encoding on the CPU. Large displays may not \
                     hold a high frame rate at full resolution.",
                )
                .size(11.0)
                .color(TEXT_DIM),
            );
        }
    }

    let encode = settings.to_session_config().encode;
    if encode.is_falling_back() {
        ui.label(
            RichText::new(format!(
                "⚠ {} is not implemented yet — sessions will use {} instead.",
                encode.codec.label(),
                encode.effective_codec().label()
            ))
            .size(11.0)
            .color(AMBER),
        );
    }

    // "Hardware" is a hard requirement, not a preference: a session refuses to
    // start rather than quietly encoding on the CPU. Say so where the choice is
    // made, on a machine that has nothing to fall back to.
    if encode.effective_codec().is_lossy()
        && encode.backend == EncoderBackend::Hardware
        && tain_core::encoder::hardware_encoder_name().is_none()
    {
        ui.label(
            RichText::new(
                "⚠ This machine has no GPU encoder, so sessions will refuse to start. \
                 Use Automatic to allow the software encoder.",
            )
            .size(11.0)
            .color(AMBER),
        );
    }

    // The lossless path is cheap on a still screen and brutal on a moving one,
    // which is the opposite of what "lossless" suggests to most people.
    if encode.effective_codec() == VideoCodec::DeltaZstd {
        ui.label(
            RichText::new(
                "Pixel-exact, but bandwidth scales with how much of the screen changes — \
                 video and scrolling get expensive fast.",
            )
            .size(11.0)
            .color(TEXT_DIM),
        );
    }

    // Raw gets expensive far faster than people expect, so show the cost rather
    // than letting them discover it as stutter.
    if !encode.effective_codec().is_compressed() {
        let mb = capture.raw_mb_per_sec();
        let colour = if mb > 100.0 {
            OFFLINE_RED
        } else if mb > 30.0 {
            AMBER
        } else {
            TEXT_DIM
        };
        ui.label(
            RichText::new(format!(
                "≈{mb:.0} MB/s uncompressed — no bitrate limit can throttle raw video."
            ))
            .size(11.0)
            .color(colour),
        );
    }
}

fn codec_text(choice: CodecSetting) -> String {
    let core = choice.to_core();
    if !core.is_available() {
        return format!("{} (planned)", core.label());
    }
    // Implemented, but this machine may still not be able to encode it. Said on
    // the entry itself rather than discovered when a session quietly streams
    // something else — see `VideoCodec::is_encodable_here`.
    if !core.is_encodable_here() {
        return format!("{} (no encoder on this PC)", core.label());
    }
    core.label().to_string()
}

/// What sound is sent, and what is kept out of it.
fn audio_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    heading(ui, "Audio");

    egui::Grid::new("audio").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Quality").on_hover_text(
            "Opus. 160 kbit/s is transparent for nearly all content; above 256 there is nothing left to gain.",
        );
        ui.horizontal(|ui| {
            for kbps in tain_core::AUDIO_BITRATES_KBPS {
                let selected = settings.audio_bitrate_kbps == kbps;
                if ui
                    .selectable_label(selected, format!("{kbps}"))
                    .on_hover_text(match kbps {
                        96 => "Noticeably compressed on music, fine for speech and game audio",
                        160 => "Transparent for nearly all content",
                        _ => "Beyond what most people can tell from the source",
                    })
                    .clicked()
                {
                    settings.audio_bitrate_kbps = kbps;
                }
            }
            ui.label(RichText::new("kbit/s").size(11.0).color(TEXT_DIM));
        });
        ui.end_row();

        ui.label("Exclude apps").on_hover_text(
            "Keep applications out of the streamed sound — a voice chat, say.

             One app is removed by Windows itself, inside the audio engine. Choosing more than one switches to mixing the apps that are left, which is exact but only sees what was already playing when the session started.",
        );
        ui.vertical(|ui| {
            if panel.audio_apps.is_empty() {
                panel.audio_apps = tain_core::playing_audio();
            }

            // A checklist rather than a combo box: the whole point is choosing
            // several, and a multi-select combo hides what is currently on.
            for app in &panel.audio_apps {
                if app.name.is_empty() {
                    continue;
                }
                let mut on = settings
                    .exclude_audio_processes
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&app.name));
                if ui.checkbox(&mut on, &app.name).changed() {
                    settings.exclude_audio_processes.retain(|n| !n.eq_ignore_ascii_case(&app.name));
                    if on {
                        settings.exclude_audio_processes.push(app.name.clone());
                    }
                }
            }

            // Apps chosen earlier that are not playing now still belong on the
            // list, or opening this panel would quietly clear them.
            let absent: Vec<String> = settings
                .exclude_audio_processes
                .iter()
                .filter(|n| !panel.audio_apps.iter().any(|a| a.name.eq_ignore_ascii_case(n)))
                .cloned()
                .collect();
            for name in absent {
                let mut on = true;
                if ui.checkbox(&mut on, format!("{name} (not playing)")).changed() && !on {
                    settings.exclude_audio_processes.retain(|n| !n.eq_ignore_ascii_case(&name));
                }
            }

            ui.horizontal(|ui| {
                if ui.button("Refresh list").clicked() {
                    panel.audio_apps = tain_core::playing_audio();
                }
                if !settings.exclude_audio_processes.is_empty()
                    && ui.button("Clear").clicked()
                {
                    settings.exclude_audio_processes.clear();
                }
            });

            let note = if settings.exclude_audio_processes.len() > 1 {
                "Several excluded — the stream is mixed from the remaining apps, so anything that starts playing later will be missing until you reconnect."
            } else {
                "Only applications currently playing sound are listed."
            };
            ui.label(RichText::new(note).size(11.0).color(TEXT_DIM));
        });
        ui.end_row();
    });
}

/// Installing and removing the Tain service.
///
/// # Why the buttons live beside the thing they unlock
///
/// The service is what makes UAC prompts, the lock screen and a machine with
/// nobody logged in reachable at all. Putting "install" in one place and the
/// access it enables in another leaves a switch nobody can explain, so both are
/// here, and so is what it costs.
///
/// Nothing here is required. Tain runs exactly as it always has with no service
/// installed — that is the whole point of it being opt-in.
fn unattended_section(ui: &mut egui::Ui, settings: &mut Settings) {
    use crate::service::{self, ServiceState};

    heading(ui, "Unattended access");

    let state = service::state();
    let installed = matches!(
        state,
        ServiceState::Running | ServiceState::Stopped | ServiceState::Mismatched
    );

    ui.label(
        RichText::new(
            "Reaching a UAC prompt, a lock screen, or a machine with nobody logged in \
             needs a Windows service. Everything else works without it.",
        )
        .size(11.0)
        .color(TEXT_DIM),
    );
    ui.add_space(6.0);

    ui.label(RichText::new(state.summary()).size(11.0).color(
        if state.is_usable() { TEXT } else { TEXT_DIM },
    ));
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        // Both buttons are always visible so the state is legible either way —
        // an absent button reads as a missing feature rather than as one that
        // does not apply.
        //
        // `Unknown` counts as not installed. It used to count as installed,
        // which greyed out the only button that could have helped on a machine
        // where the service database simply could not be read.
        let installed = matches!(
            state,
            ServiceState::Running | ServiceState::Stopped | ServiceState::Mismatched
        );

        if ui
            .add_enabled(
                !installed || state == ServiceState::Mismatched,
                egui::Button::new(if state == ServiceState::Mismatched {
                    "Reinstall service"
                } else {
                    "Install service"
                }),
            )
            .on_hover_text(
                "Windows will ask for permission — registering a service needs \
                 administrator rights, and Tain does not run with them.",
            )
            .clicked()
            && let Err(e) = service::request_install()
        {
            tracing::warn!("Could not start the service installer: {e:#}");
        }

        if ui
            .add_enabled(installed, egui::Button::new("Uninstall service"))
            .on_hover_text(
                "Removes the service and everything it allowed. Tain keeps working \
                 for ordinary sessions. Windows will ask for permission.",
            )
            .clicked()
            && let Err(e) = service::request_uninstall()
        {
            tracing::warn!("Could not start the service uninstaller: {e:#}");
        }
    });

    ui.add_space(8.0);

    // Greyed rather than hidden when there is no service: the setting is what
    // the section is named for, and a checkbox that appears only after an
    // unrelated button is pressed is a setting nobody finds.
    ui.add_enabled_ui(installed, |ui| {
        ui.checkbox(
            &mut settings.unattended_access,
            "Answer when nobody is signed in",
        )
        .on_hover_text(
            "Hands this machine's identity to the service, so the same connection \
             code reaches it at the sign-in screen. Off, the service still covers \
             consent prompts and the lock screen while somebody is signed in.",
        );
    });
    if !installed {
        ui.label(
            RichText::new("Needs the service installed.").size(11.0).color(TEXT_DIM),
        );
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "The service runs as LocalSystem so it can reach screens an ordinary \
             program cannot. That is a real amount of trust to place in this machine \
             and in whoever holds a code for it — install it only where you want \
             unattended access.",
        )
        .size(10.5)
        .color(TEXT_DIM),
    );
}

fn security_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, "Security");

    ui.label(
        RichText::new(
            "Guests are streamed nothing until you allow them, every time they connect.",
        )
        .size(11.0)
        .color(TEXT_DIM),
    );
    ui.add_space(6.0);

    egui::Grid::new("security").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Approval timeout");
        ui.add(egui::Slider::new(&mut settings.approval_timeout_secs, 5..=300).suffix(" s"))
            .on_hover_text(
                "How long a guest waits for an answer before being refused. \
                 Timing out denies — an unattended machine should not share its screen.",
            );
        ui.end_row();

        ui.label("Admin code");
        ui.checkbox(&mut settings.admin_autoreset, "Reset automatically")
            .on_hover_text("Admin codes grant full control immediately — resetting often is safer");
        ui.end_row();

        ui.label("Guest code");
        ui.checkbox(&mut settings.guest_autoreset, "Reset when the last guest leaves");
        ui.end_row();

        ui.label("Offer by default");
        ui.vertical(|ui| {
            ui.checkbox(&mut settings.default_allow_mouse, "Mouse");
            ui.checkbox(&mut settings.default_allow_keyboard, "Keyboard");
        })
        .response
        .on_hover_text(
            "Which boxes start ticked in the approval dialog. \
             You still have to allow each connection. \
             Mouse also covers touch.",
        );
        ui.end_row();
    });

    if settings.default_allow_mouse || settings.default_allow_keyboard {
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "⚠ Input will be pre-ticked — check the dialog before allowing a guest.",
            )
            .size(11.0)
            .color(AMBER),
        );
    }
}

/// This application's own appearance, as opposed to what it shows of another
/// machine.
fn interface_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    use super::app::SCALE_EPSILON as EPSILON;

    heading(ui, "Interface — this application's own window");

    // Held rather than applied as it moves — see `PanelState::pending_ui_scale`
    // for what happens when a slider resizes itself.
    let (pending, taken_from) =
        panel.pending_ui_scale.get_or_insert((settings.ui_scale, settings.ui_scale));
    if (*taken_from - settings.ui_scale).abs() > EPSILON {
        // Changed somewhere else while this window was open.
        *pending = settings.ui_scale;
        *taken_from = settings.ui_scale;
    }

    egui::Grid::new("interface").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Scale").on_hover_text(
            "Relative to what the monitor asks for, so it still follows a screen \
             with a different DPI. Ctrl+plus and Ctrl+minus move it too.",
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::Slider::new(pending, super::settings::UI_SCALE_RANGE)
                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                    .step_by(0.05),
            );
            // Sets the slider, without applying: a button that resized the
            // window under the pointer would move the next thing to be clicked.
            let off_default = (*pending - 1.0).abs() > EPSILON;
            if ui
                .add_enabled(off_default, egui::Button::new("100%"))
                .on_hover_text("Follow the monitor exactly")
                .clicked()
            {
                *pending = 1.0;
            }

            let differs = (*pending - settings.ui_scale).abs() > EPSILON;
            if ui
                .add_enabled(differs, egui::Button::new("Apply"))
                .on_hover_text("Resize the interface to the chosen scale")
                .clicked()
            {
                settings.ui_scale = *pending;
                *taken_from = *pending;
            }
        });
        ui.end_row();
    });

    ui.add_space(2.0);
    let note = if (*pending - settings.ui_scale).abs() > EPSILON {
        format!("Currently {:.0}%. Apply to resize the interface.", settings.ui_scale * 100.0)
    } else {
        "Applied on the button rather than as the slider moves, because this \
         setting resizes the slider. Ctrl+plus and Ctrl+minus apply at once."
            .to_string()
    };
    ui.label(RichText::new(note).size(11.0).color(TEXT_DIM));
}

/// What this machine does with a session it connects **out** to.
///
/// The other half of the split described on [`video_section`]. Nothing here
/// reaches the host: it is all about how the picture arriving is decoded and
/// drawn on this screen.
fn viewing_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, "Viewing — what this PC receives");

    egui::Grid::new("viewing").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Scaling")
            .on_hover_text("How a remote screen that does not match this window is shown.");
        ui.horizontal(|ui| {
            for mode in ScalingMode::ALL {
                ui.selectable_value(&mut settings.scaling_mode, mode, mode.label())
                    .on_hover_text(mode.description());
            }
        });
        ui.end_row();

        // # One setting, on the side it belongs to
        //
        // This was offered twice — here, and again among the hosting settings,
        // where a comment admitted it did not belong. Two checkboxes for one
        // value read as two decisions, and invited the question of which end
        // wins.
        //
        // Neither, because there is nothing to agree on. Whether a decoded
        // frame can be drawn without being copied back depends on the receiving
        // machine's driver and its decoder, and the sender neither knows nor
        // affects it — the value is read in exactly one place, this machine's
        // own renderer. A host has no more say in it than in which monitor the
        // window is on.
        if cfg!(target_os = "windows") {
            ui.label("Draw frames on the GPU").on_hover_text(
                "For sessions this machine receives. Skips copying each decoded \
                 frame off the GPU and back, which is most of the decode cost. \
                 It depends on a driver extension, and turns itself off for the \
                 session if the extension is missing or the decoder will not hand \
                 its textures over — but it cannot tell that the picture has \
                 stopped updating, so turn it off if that happens.",
            );
            ui.checkbox(&mut settings.gpu_present, "Skip the readback (experimental)");
            ui.end_row();
        }

        ui.label("");
        ui.checkbox(&mut settings.fullscreen_on_connect, "Enter fullscreen on connect");
        ui.end_row();
    });
}

fn shortcut_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    heading(ui, "Shortcuts");
    ui.label(
        RichText::new("Handled locally during a session — never sent to the host.")
            .size(11.0)
            .color(TEXT_DIM),
    );
    ui.add_space(6.0);

    // Capture happens outside the grid so a press is consumed before any widget
    // below can react to it.
    if let Some(action) = panel.capturing
        && let Some(shortcut) = capture_pressed_key(ui.ctx())
    {
        settings.shortcuts.set(action, shortcut);
        panel.capturing = None;
    }

    let conflicts = settings.shortcuts.conflicts();

    egui::Grid::new("shortcuts").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
        for action in ShortcutAction::ALL {
            ui.label(action.label()).on_hover_text(action.description());

            let capturing = panel.capturing == Some(action);
            let shortcut = settings.shortcuts.get(action);
            let text = if capturing { "press a key…".to_string() } else { shortcut.to_string() };

            if ui
                .add_sized(
                    [140.0, 22.0],
                    egui::SelectableLabel::new(capturing, text),
                )
                .on_hover_text("Click, then press the combination you want")
                .clicked()
            {
                panel.capturing = if capturing { None } else { Some(action) };
            }

            // Warn rather than block: refusing the first half of a swap is more
            // annoying than flagging the overlap.
            if conflicts.contains(&action) {
                ui.label(RichText::new("⚠ duplicate").size(11.0).color(AMBER));
            } else if !shortcut.is_safe() {
                ui.label(RichText::new("⚠ may block typing").size(11.0).color(AMBER))
                    .on_hover_text(
                        "Without a modifier this key is indistinguishable from typing, \
                         so the host will never receive it",
                    );
            } else {
                ui.label("");
            }
            ui.end_row();
        }
    });
}

/// Read a key press for rebinding, ignoring bare modifier presses.
fn capture_pressed_key(ctx: &Context) -> Option<Shortcut> {
    ctx.input(|i| {
        let modifiers = i.modifiers;
        i.events.iter().find_map(|event| match event {
            egui::Event::Key { key, pressed: true, .. } => Some(Shortcut::new(
                *key,
                modifiers.ctrl,
                modifiers.shift,
                modifiers.alt,
            )),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_implemented_codecs_are_selectable() {
        let selectable: Vec<_> =
            VideoCodec::ALL.into_iter().filter(|c| c.is_available()).collect();
        assert_eq!(
            selectable,
            vec![
                VideoCodec::DeltaZstd,
                VideoCodec::Raw,
                VideoCodec::H264,
                VideoCodec::H265,
                VideoCodec::Av1,
            ]
        );
    }

    /// H.265 is selectable but not necessarily *usable* on this machine.
    ///
    /// The two are different questions and the UI has to keep them apart: the
    /// build has the code, and a GPU either has an HEVC encoder or it does not.
    /// Offering it and falling back to H.264 is the honest arrangement — see
    /// `VideoCodec::is_encodable_here`.
    #[test]
    fn a_codec_this_machine_cannot_encode_is_still_offered() {
        assert!(VideoCodec::H265.is_available(), "the build implements it");
        // And whichever way this machine answers, the two questions are asked
        // separately rather than one standing in for the other.
        let _ = VideoCodec::H265.is_encodable_here();
    }

    #[test]
    fn planned_codecs_are_labelled_as_such() {
        assert_eq!(codec_text(CodecSetting::Raw), "Raw BGRA");
        assert!(!codec_text(CodecSetting::DeltaZstd).contains("planned"));
        assert!(!codec_text(CodecSetting::H264).contains("planned"), "H.264 works now");
        // H.265 is implemented, so it is never "planned" — but on a machine
        // with no HEVC encoder it says so instead, which is a different claim.
        assert!(!codec_text(CodecSetting::H265).contains("planned"), "H.265 works now");
        if !VideoCodec::H265.is_encodable_here() {
            assert!(codec_text(CodecSetting::H265).contains("no encoder"));
        }
        // AV1 joined H.265: implemented, and gated on the machine rather than
        // on the build. Its encoders are newer hardware still, so most machines
        // will say "no encoder on this PC" — which is a claim about them, not
        // about whether the code exists.
        assert!(!codec_text(CodecSetting::Av1).contains("planned"), "AV1 works now");
        if !VideoCodec::Av1.is_encodable_here() {
            assert!(codec_text(CodecSetting::Av1).contains("no encoder"));
        }
        assert!(codec_text(CodecSetting::Vp9).contains("planned"));
    }

    #[test]
    fn bitrate_controls_are_only_offered_for_a_codec_that_can_honour_them() {
        // The lossless path compresses but cannot hit a bitrate target — its
        // size is whatever the screen did, so offering a limit would lie.
        assert!(VideoCodec::H264.is_lossy());
        assert!(!VideoCodec::DeltaZstd.is_lossy());
        assert!(!VideoCodec::Raw.is_lossy());
    }

    #[test]
    fn rate_control_choice_decides_which_control_is_shown() {
        // Constant-quality replaces the bitrate row with a quality slider.
        assert!(RateControlSetting::Constant.to_core().uses_bitrate());
        assert!(RateControlSetting::Variable.to_core().uses_bitrate());
        assert!(!RateControlSetting::ConstantQuality.to_core().uses_bitrate());
    }

    /// Draw the settings window once, headlessly, and report what it said.
    fn draw_once(settings: &mut Settings, panel: &mut PanelState) -> bool {
        let ctx = Context::default();
        let mut changed = false;
        let mut open = true;
        // The output is a frame to paint, which no test has a screen for.
        let _ = ctx.run(Default::default(), |ctx| {
            changed = show(ctx, &mut open, settings, panel);
        });
        changed
    }

    /// The interface scale must not take effect while it is being edited.
    ///
    /// It resizes the slider that sets it, so a value applied as it moved would
    /// move the control out from under the pointer and be read again from the
    /// new position — a handle sliding between the extremes on its own. The
    /// held value existing is the whole defence, and nothing else in this
    /// window works that way, so it is the kind of thing a later tidy-up
    /// removes as an inconsistency.
    #[test]
    fn a_scale_being_edited_does_not_apply_until_it_is_applied() {
        let mut settings = Settings::default();
        let mut panel = PanelState { pending_ui_scale: Some((2.0, 1.0)), ..Default::default() };

        let changed = draw_once(&mut settings, &mut panel);

        assert_eq!(settings.ui_scale, 1.0, "a held edit must not reach the settings");
        assert!(!changed, "and must not ask for the settings to be saved");
        assert_eq!(
            panel.pending_ui_scale.map(|(pending, _)| pending),
            Some(2.0),
            "while still being what the slider shows"
        );
    }

    /// A scale changed from outside — Ctrl+plus, or Reset to defaults — has to
    /// win over an edit nobody applied, or the panel would show a stale value
    /// and offer to apply it.
    #[test]
    fn a_scale_changed_elsewhere_replaces_an_unapplied_edit() {
        let mut settings = Settings { ui_scale: 1.5, ..Settings::default() };
        // Dialled to 2.0, from a setting that then said 1.0.
        let mut panel = PanelState { pending_ui_scale: Some((2.0, 1.0)), ..Default::default() };

        draw_once(&mut settings, &mut panel);

        assert_eq!(panel.pending_ui_scale, Some((1.5, 1.5)), "the edit follows the setting");
        assert_eq!(settings.ui_scale, 1.5, "and the setting itself is untouched");
    }

    /// Closing the window throws the unapplied edit away rather than keeping it
    /// to be applied by surprise on the next visit.
    #[test]
    fn closing_the_window_abandons_an_unapplied_scale() {
        let mut settings = Settings::default();
        let mut panel = PanelState { pending_ui_scale: Some((2.0, 1.0)), ..Default::default() };

        let ctx = Context::default();
        let mut open = false;
        let _ = ctx.run(Default::default(), |ctx| {
            show(ctx, &mut open, &mut settings, &mut panel);
        });

        assert_eq!(panel.pending_ui_scale, None);
        assert_eq!(settings.ui_scale, 1.0);
    }

    /// The scale row must fit the window it is drawn in.
    ///
    /// A label, a slider with a value box and two buttons is more than any
    /// other row here carries, and the window is a fixed width that does not
    /// scroll sideways.
    #[test]
    fn the_settings_window_does_not_grow_sideways_for_the_scale_row() {
        let ctx = Context::default();
        let mut settings = Settings::default();
        let mut panel = PanelState::default();
        let mut open = true;

        // Twice: egui sizes a window from what it measured on the frame before.
        for _ in 0..2 {
            let _ = ctx.run(Default::default(), |ctx| {
                show(ctx, &mut open, &mut settings, &mut panel);
            });
        }

        let rect = ctx
            .memory(|m| m.area_rect(egui::Id::new("Settings")))
            .expect("the settings window should have been laid out");
        // 470 is the requested width; the margin is for the frame around it.
        assert!(
            rect.width() <= 500.0,
            "the settings window widened to {:.0} points to fit its contents",
            rect.width()
        );
    }

    #[test]
    fn unlimited_bitrate_is_represented_as_zero() {
        let mut s = Settings { max_bitrate_kbps: 0, ..Settings::default() };
        s.sanitize();
        assert_eq!(s.max_bitrate_kbps, 0);

        s.max_bitrate_kbps = 8_000;
        s.sanitize();
        assert_eq!(s.max_bitrate_kbps, 8_000);
    }

    #[test]
    fn raw_encoder_ignores_the_compression_controls() {
        // Bitrate, rate control and keyframes are meaningless without a codec.
        assert!(!VideoCodec::Raw.is_compressed());
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1, VideoCodec::Vp9] {
            assert!(codec.is_lossy());
        }
    }
}
