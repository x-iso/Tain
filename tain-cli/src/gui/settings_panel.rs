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
// Every section in this window draws text, so the catalogue is reached for
// here rather than in each of them.
use tain_core::i18n;
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

    // A fixed id, because the title is translated and egui keys a window's
    // remembered position and size by it — changing language would otherwise
    // hand the window a fresh identity and lose where it was.
    egui::Window::new(i18n::text("header.settings"))
        .id(egui::Id::new("settings-window"))
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
                    language_section(ui, settings);
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
                    ui.add_space(18.0);
                    virtual_display_section(ui, settings);
                    ui.add_space(10.0);
                    shortcut_section(ui, settings, panel);
                });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tain_core::i18n::text("settings.applies_to_new"))
                        .size(11.0)
                        .color(TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(tain_core::i18n::text("settings.reset_defaults")).clicked() {
                        *settings = Settings::default();
                        panel.capturing = None;
                    }
                    // Here rather than only in the log, because "which version
                    // are you on" is the first question about any report, and
                    // the person answering it should not need to find a file.
                    ui.label(
                        RichText::new(i18n::format(
                            "settings.version",
                            &[("version", env!("CARGO_PKG_VERSION"))],
                        ))
                        .size(11.0)
                        .color(TEXT_DIM),
                    );
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
    heading(ui, i18n::text("settings.identity"));

    egui::Grid::new("identity").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(i18n::text("settings.identity.name"));
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
                    .hint_text(i18n::text("settings.identity.custom_hint")),
            );
            ui.end_row();
        }
    });

    ui.add_space(4.0);
    // Preview the exact string the other machine will see, after the same
    // cleaning the receiving end applies. "Anonymous" is not a name being sent —
    // it means no name is sent at all.
    let preview = match settings.device_name() {
        Some(name) => i18n::format("settings.identity.shown_as", &[("name", &name)]),
        None => i18n::text("settings.identity.no_name").to_string(),
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
    heading(ui, i18n::text("settings.video"));

    let codec = settings.codec.to_core();

    egui::Grid::new("video").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(i18n::text("settings.video.codec"));
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

        ui.label(i18n::text("settings.video.encoder"));
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
            ui.label(i18n::text("settings.video.gpu"))
                .on_hover_text(i18n::text("settings.video.gpu_hint"));
            let selected = if settings.preferred_encoder.is_empty() {
                i18n::text("settings.video.automatic").to_string()
            } else if choices.iter().any(|(key, _)| *key == settings.preferred_encoder) {
                settings.preferred_encoder.clone()
            } else {
                // A preference carried over from another machine. Said plainly
                // rather than silently reset, so it is clear why the stream is
                // not on the card the settings claim.
                i18n::format(
                    "settings.video.not_on_this_machine",
                    &[("name", &settings.preferred_encoder)],
                )
            };
            egui::ComboBox::from_id_salt("preferred_encoder")
                .width(220.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut settings.preferred_encoder,
                        String::new(),
                        i18n::text("settings.video.automatic"),
                    )
                    .on_hover_text(i18n::text("settings.video.automatic_hint"));
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
            ui.label(i18n::text("settings.video.profile"))
                .on_hover_text(i18n::text("settings.video.profile_hint"));
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
                    RichText::new(i18n::text("settings.video.high_profile_warning"))
                        .size(11.0)
                        .color(AMBER),
                );
                ui.end_row();
            }
        }

        ui.label(i18n::text("settings.video.resolution"))
            .on_hover_text(i18n::text("settings.video.resolution_hint"));
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

        ui.label(i18n::text("settings.video.frame_rate"))
            .on_hover_text(i18n::text("settings.video.frame_rate_hint"));
        ui.add(egui::Slider::new(&mut settings.target_fps, 5..=144).suffix(" fps"));
        ui.end_row();

        ui.label(i18n::text("settings.video.if_behind"));
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
                ui.label(
                    RichText::new(i18n::text("settings.video.keep_detail"))
                        .size(10.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(104.0);
                ui.label(
                    RichText::new(i18n::text("settings.video.keep_motion"))
                        .size(10.0)
                        .color(TEXT_DIM),
                );
            });
        });
        ui.end_row();

        ui.label("");
        ui.checkbox(
            &mut settings.follow_priority,
            i18n::text("settings.video.follow_priority"),
        )
            .on_hover_text(i18n::text("settings.video.auto_quality_hint"));
        ui.end_row();

        ui.label(i18n::text("settings.video.rate_control"));
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
            ui.label(i18n::text("settings.video.bitrate_limit"));
            ui.add_enabled_ui(codec.is_lossy(), |ui| {
                ui.horizontal(|ui| {
                    let mut unlimited = settings.max_bitrate_kbps == 0;
                    if ui
                        .checkbox(&mut unlimited, i18n::text("settings.video.unlimited"))
                        .changed()
                    {
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
                ui.label(i18n::text("settings.video.quality_target"));
                ui.add(egui::Slider::new(&mut settings.encode_quality, 0..=100));
                ui.end_row();
            }

            ui.label(i18n::text("settings.video.keyframe_interval"));
            ui.add_enabled_ui(codec.is_lossy(), |ui| {
                ui.add(
                    egui::Slider::new(&mut settings.keyframe_interval, 0..=600)
                        .suffix(" frames")
                        .custom_formatter(|n, _| {
                            if n == 0.0 {
                                i18n::text("settings.video.keyframe_on_request").into()
                            } else {
                                format!("{n:.0}")
                            }
                        }),
                )
                .on_hover_text(i18n::text("settings.video.keyframe_hint"));
            });
            ui.end_row();
        }

        ui.label(i18n::text("settings.video.capture_cursor"));
        ui.checkbox(&mut settings.show_cursor, i18n::text("settings.video.include_pointer"));
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
    let fps = capture.fps.to_string();
    let (nw, nh, sw, sh) = (nw.to_string(), nh.to_string(), sw.to_string(), sh.to_string());
    let note = if (&nw, &nh) == (&sw, &sh) {
        i18n::format(
            "settings.video.summary_full",
            &[("w", &nw), ("h", &nh), ("fps", &fps)],
        )
    } else {
        i18n::format(
            "settings.video.summary_scaled",
            &[("w", &nw), ("h", &nh), ("sw", &sw), ("sh", &sh), ("fps", &fps)],
        )
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
                RichText::new(i18n::format("settings.video.encoding_via", &[("name", &name)]))
                    .size(11.0)
                    .color(TEXT_DIM),
            );
        }
        Some(_) => {
            ui.label(
                RichText::new(i18n::text("settings.video.gpu_but_software"))
                    .size(11.0)
                    .color(AMBER),
            );
        }
        None => {
            ui.label(
                RichText::new(i18n::text("settings.video.no_gpu_encoder"))
                    .size(11.0)
                    .color(TEXT_DIM),
            );
        }
    }

    let encode = settings.to_session_config().encode;
    if encode.is_falling_back() {
        ui.label(
            RichText::new(i18n::format(
                "settings.video.not_implemented",
                &[
                    ("wanted", encode.codec.label()),
                    ("instead", encode.effective_codec().label()),
                ],
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
            RichText::new(i18n::text("settings.video.hardware_only_no_gpu"))
                .size(11.0)
                .color(AMBER),
        );
    }

    // The lossless path is cheap on a still screen and brutal on a moving one,
    // which is the opposite of what "lossless" suggests to most people.
    if encode.effective_codec() == VideoCodec::DeltaZstd {
        ui.label(
            RichText::new(i18n::text("settings.video.lossless_cost"))
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
            RichText::new(i18n::format(
                "settings.video.raw_bandwidth",
                &[("mb", &format!("{mb:.0}"))],
            ))
            .size(11.0)
            .color(colour),
        );
    }
}

fn codec_text(choice: CodecSetting) -> String {
    let core = choice.to_core();
    if !core.is_available() {
        return i18n::format("settings.video.planned", &[("name", core.label())]);
    }
    // Implemented, but this machine may still not be able to encode it. Said on
    // the entry itself rather than discovered when a session quietly streams
    // something else — see `VideoCodec::is_encodable_here`.
    if !core.is_encodable_here() {
        return i18n::format("settings.video.no_encoder", &[("name", core.label())]);
    }
    core.label().to_string()
}

/// What sound is sent, and what is kept out of it.
fn audio_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    heading(ui, i18n::text("settings.audio"));

    egui::Grid::new("audio").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(i18n::text("settings.audio.quality"))
            .on_hover_text(i18n::text("settings.audio.quality_hint"));
        ui.horizontal(|ui| {
            for kbps in tain_core::AUDIO_BITRATES_KBPS {
                let selected = settings.audio_bitrate_kbps == kbps;
                if ui
                    .selectable_label(selected, format!("{kbps}"))
                    .on_hover_text(i18n::text(match kbps {
                        96 => "settings.audio.q96",
                        160 => "settings.audio.q160",
                        _ => "settings.audio.q_high",
                    }))
                    .clicked()
                {
                    settings.audio_bitrate_kbps = kbps;
                }
            }
            ui.label(
                RichText::new(i18n::text("settings.audio.kbits")).size(11.0).color(TEXT_DIM),
            );
        });
        ui.end_row();

        ui.label(i18n::text("settings.audio.exclude"))
            .on_hover_text(i18n::text("settings.audio.exclude_hint"));
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
                let shown = i18n::format("settings.audio.not_playing", &[("name", &name)]);
                if ui.checkbox(&mut on, shown).changed() && !on {
                    settings.exclude_audio_processes.retain(|n| !n.eq_ignore_ascii_case(&name));
                }
            }

            ui.horizontal(|ui| {
                if ui.button(i18n::text("settings.audio.refresh")).clicked() {
                    panel.audio_apps = tain_core::playing_audio();
                }
                if !settings.exclude_audio_processes.is_empty()
                    && ui.button(i18n::text("settings.audio.clear")).clicked()
                {
                    settings.exclude_audio_processes.clear();
                }
            });

            let note = i18n::text(if settings.exclude_audio_processes.len() > 1 {
                "settings.audio.several_excluded"
            } else {
                "settings.audio.only_playing_listed"
            });
            ui.label(RichText::new(note).size(11.0).color(TEXT_DIM));
        });
        ui.end_row();
    });
}

/// Installing and removing the virtual display driver.
///
/// # Why this is its own section rather than a line in the one above
///
/// Both install something and both need administrator rights, which is most of
/// what they have in common. The service makes an *existing* desktop reachable
/// when nobody is signed in; this creates a desktop that was not there. Someone
/// who wants one very often does not want the other — a headless box wants
/// both, a laptop being used as a spare screen wants only this — and a single
/// "install everything" button would make that impossible to say.
///
/// # Why the state is read every frame
///
/// It changes underneath this window: the install runs in a separate elevated
/// process, so the moment it finishes is not a moment this code hears about.
/// Asking each frame costs an enumeration of display adapters, which is
/// microseconds, and means the panel is right without anything to keep in step.
fn virtual_display_section(ui: &mut egui::Ui, settings: &mut Settings) {
    use crate::display_driver::{self, DriverState};

    heading(ui, i18n::text("settings.vdisplay"));

    let state = display_driver::state();

    ui.label(
        RichText::new(i18n::text("settings.vdisplay.blurb")).size(11.0).color(TEXT_DIM),
    );
    ui.add_space(6.0);

    ui.label(
        RichText::new(state.summary())
            .size(11.0)
            .color(if state.is_usable() { TEXT } else { TEXT_DIM }),
    );
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        // Both buttons always drawn, as in the section above: a button that
        // disappears reads as a feature that is missing rather than one that
        // does not apply right now.
        if ui
            .add_enabled(
                state == DriverState::NotInstalled,
                egui::Button::new(i18n::text("settings.vdisplay.install")),
            )
            .on_hover_text(i18n::text("settings.vdisplay.install_hint"))
            .clicked()
            && let Err(e) = display_driver::request_install()
        {
            tracing::warn!("Could not start the display driver installer: {e:#}");
        }

        if ui
            .add_enabled(
                state.is_installed(),
                egui::Button::new(i18n::text("settings.vdisplay.remove_driver")),
            )
            .on_hover_text(i18n::text("settings.vdisplay.remove_driver_hint"))
            .clicked()
            && let Err(e) = display_driver::request_uninstall()
        {
            tracing::warn!("Could not start the display driver uninstaller: {e:#}");
        }
    });

    // Said only where it is the answer. On a machine that can install, this
    // line is noise; on one that cannot, its absence is a button that does
    // nothing for a reason nobody can see.
    if state == DriverState::NotBundled {
        ui.add_space(6.0);
        ui.label(
            RichText::new(i18n::text("settings.vdisplay.not_bundled"))
                .size(11.0)
                .color(TEXT_DIM),
        );
        return;
    }

    // # The displays themselves, once there is a driver to make them
    //
    // Shown only when installed, because until then there is nothing to count
    // and a stepper reading zero looks like a feature that is failing rather
    // than one that has not been set up.
    //
    // Adding needs no administrator rights — the driver's control pipe grants
    // access to everyone — so unlike the buttons above, these act immediately
    // and there is no prompt.
    if !state.is_installed() {
        return;
    }

    ui.add_space(10.0);
    let count = display_driver::monitor_count();
    ui.label(
        RichText::new(match count {
            0 => i18n::text("settings.vdisplay.count.zero").to_string(),
            n => i18n::plural("settings.vdisplay.count", n as u64, &[]),
        })
        .size(11.0)
        .color(if count > 0 { TEXT } else { TEXT_DIM }),
    );
    ui.add_space(8.0);

    // # The size, and who decides it
    //
    // Three answers, in the order somebody would think of them: match whoever
    // is watching, match the screen already here, or say. The first is the
    // reason the feature exists — a display made the size of the guest's window
    // is sharp at 1:1, where a scaled copy of a 2560x1600 desktop never is.
    ui.checkbox(
        &mut settings.virtual_display_follows_guest,
        i18n::text("settings.vdisplay.follow_guest"),
    )
    .on_hover_text(i18n::text("settings.vdisplay.follow_guest_hint"));

    ui.add_space(4.0);
    ui.add_enabled_ui(!settings.virtual_display_follows_guest, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(i18n::text("settings.vdisplay.size")).size(11.0).color(TEXT_DIM),
            );

            // Offered from what a display actually reports, so the list cannot
            // contain a mode the driver will refuse. Falls back to the primary
            // display's size, which is what zero means.
            let primary = tain_core::screen_capture::primary_display_size();
            let mut sizes = display_driver::offered_sizes();
            if !sizes.contains(&primary) {
                sizes.insert(0, primary);
            }

            let chosen = settings.virtual_display_size;
            let label = if chosen == (0, 0) {
                i18n::format(
                    "settings.vdisplay.same_as_screen",
                    &[("w", &primary.0.to_string()), ("h", &primary.1.to_string())],
                )
            } else {
                format!("{}x{}", chosen.0, chosen.1)
            };

            egui::ComboBox::from_id_salt("virtual-display-size")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut settings.virtual_display_size,
                        (0, 0),
                        i18n::format(
                    "settings.vdisplay.same_as_screen",
                    &[("w", &primary.0.to_string()), ("h", &primary.1.to_string())],
                ),
                    );
                    for size in sizes {
                        ui.selectable_value(
                            &mut settings.virtual_display_size,
                            size,
                            format!("{}x{}", size.0, size.1),
                        );
                    }
                });
        });
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        // The driver restarts itself to change the count, so both of these
        // block for a moment. Disabled rather than spinner'd: the wait is a
        // couple of seconds and a button that cannot be pressed twice is
        // simpler than a state machine for something this short.
        if ui
            .button(i18n::text("settings.vdisplay.add"))
            .on_hover_text(i18n::text("settings.vdisplay.add_hint"))
            .clicked()
        {
            // What the settings above say. Zero means "the same as this
            // screen", and a display that follows its guest still has to start
            // somewhere — it is resized when one arrives, not before.
            let size = match settings.virtual_display_size {
                (0, 0) => tain_core::screen_capture::primary_display_size(),
                chosen => chosen,
            };
            match display_driver::add_display(size) {
                Ok(name) => tracing::info!("Added virtual display {name}"),
                Err(e) => tracing::warn!("Could not add a virtual display: {e:#}"),
            }
        }

        if ui
            .add_enabled(count > 0, egui::Button::new(i18n::text("settings.vdisplay.remove")))
            .on_hover_text(i18n::text("settings.vdisplay.remove_hint"))
            .clicked()
            && let Err(e) = display_driver::remove_display()
        {
            tracing::warn!("Could not remove a virtual display: {e:#}");
        }
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

    heading(ui, i18n::text("settings.unattended"));

    let state = service::state();
    let installed = matches!(
        state,
        ServiceState::Running | ServiceState::Stopped | ServiceState::Mismatched
    );

    ui.label(
        RichText::new(i18n::text("settings.unattended.blurb")).size(11.0).color(TEXT_DIM),
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
                    i18n::text("settings.unattended.reinstall")
                } else {
                    i18n::text("settings.unattended.install")
                }),
            )
            .on_hover_text(i18n::text("settings.unattended.install_hint"))
            .clicked()
            && let Err(e) = service::request_install()
        {
            tracing::warn!("Could not start the service installer: {e:#}");
        }

        if ui
            .add_enabled(
                installed,
                egui::Button::new(i18n::text("settings.unattended.uninstall")),
            )
            .on_hover_text(i18n::text("settings.unattended.uninstall_hint"))
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
            i18n::text("settings.unattended.answer_signed_out"),
        )
        .on_hover_text(i18n::text("settings.unattended.answer_hint"));
    });
    if !installed {
        ui.label(
            RichText::new(i18n::text("settings.unattended.needs_service"))
                .size(11.0)
                .color(TEXT_DIM),
        );
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(i18n::text("settings.unattended.trust_note")).size(10.5).color(TEXT_DIM),
    );
}

/// What language the interface speaks.
///
/// # Why "Automatic" names the language it would pick
///
/// A picker whose first entry says only "Automatic" tells somebody nothing
/// about what they are choosing between — and the people most likely to open
/// this are the ones for whom automatic guessed wrong. Naming the answer makes
/// the entry a statement rather than a question.
///
/// Every language names itself in its own language, because nobody looks for
/// their own under an English name.
fn language_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, i18n::text("settings.language"));

    ui.label(RichText::new(i18n::text("settings.language.blurb")).size(11.0).color(TEXT_DIM));
    ui.add_space(6.0);

    let automatic = match i18n::automatic() {
        Some(language) => i18n::format("settings.language.automatic", &[("name", &language.name)]),
        None => i18n::text("settings.language.automatic_unknown").to_string(),
    };
    let selected = i18n::languages()
        .iter()
        .find(|l| l.tag.eq_ignore_ascii_case(&settings.language))
        .map(|l| l.name.clone())
        .unwrap_or_else(|| automatic.clone());

    egui::ComboBox::from_id_salt("language")
        .width(220.0)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.language, String::new(), automatic);
            for language in i18n::languages() {
                ui.selectable_value(
                    &mut settings.language,
                    language.tag.clone(),
                    &language.name,
                );
            }
        });

    ui.add_space(4.0);
    ui.label(
        RichText::new(i18n::text("settings.language.contribute")).size(11.0).color(TEXT_DIM),
    );
}

fn security_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, i18n::text("settings.security"));

    ui.label(RichText::new(i18n::text("settings.security.blurb")).size(11.0).color(TEXT_DIM));
    ui.add_space(6.0);

    egui::Grid::new("security").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(i18n::text("settings.security.approval_timeout"));
        ui.add(egui::Slider::new(&mut settings.approval_timeout_secs, 5..=300).suffix(" s"))
            .on_hover_text(i18n::text("settings.security.approval_timeout_hint"));
        ui.end_row();

        ui.label(i18n::text("settings.security.admin_code"));
        ui.checkbox(&mut settings.admin_autoreset, i18n::text("settings.security.admin_autoreset"))
            .on_hover_text(i18n::text("settings.security.admin_autoreset_hint"));
        ui.end_row();

        ui.label(i18n::text("settings.security.guest_code"));
        ui.checkbox(&mut settings.guest_autoreset, i18n::text("settings.security.guest_autoreset"));
        ui.end_row();

        // # Who wins when both people reach for the mouse
        //
        // A host being streamed is still a computer somebody may be sitting at,
        // and two hands on one cursor is not a shared session — it is neither
        // person being able to do anything, with the one actually in the room
        // losing, because a guest's events arrive faster than a hand moves.
        ui.label(i18n::text("settings.security.give_way"));
        ui.add(
            egui::Slider::new(&mut settings.local_input_wins_ms, 0..=10_000)
                .suffix(" ms")
                .step_by(100.0),
        )
        .on_hover_text(i18n::text("settings.security.give_way_hint"));
        ui.end_row();

        ui.label(i18n::text("settings.security.offer_by_default"));
        ui.vertical(|ui| {
            ui.checkbox(&mut settings.default_allow_mouse, i18n::text("approval.mouse"));
            ui.checkbox(&mut settings.default_allow_keyboard, i18n::text("approval.keyboard"));
        })
        .response
        .on_hover_text(i18n::text("settings.security.offer_hint"));
        ui.end_row();

        // # A default, not the value in use
        //
        // What it seeds is per session and adjustable there — the right number
        // depends on the game at the other end, and there is no way to know it
        // from here. This is where somebody who always wants the same starting
        // point puts it. See `Settings::pointer_sensitivity`.
        ui.label(i18n::text("session.pointer_sensitivity"));
        ui.add(
            egui::Slider::new(
                &mut settings.pointer_sensitivity,
                super::settings::POINTER_SENSITIVITY_RANGE,
            )
            .logarithmic(true)
            .fixed_decimals(2),
        )
        .on_hover_text(i18n::text("settings.security.sensitivity_hint"));
        ui.end_row();

        // # Who has already been let in with the admin code
        //
        // The first admin connection from each device has to be approved here,
        // and the answer is remembered so the owner is not asked again from
        // their own machines. A remembered decision nobody can see or undo is
        // not something to ask people to trust, so this says how many there are
        // and offers to forget them.
        //
        // A count rather than a list of endpoint ids: sixty-four hex characters
        // identifies a device to the protocol and to nobody at all.
        ui.label(i18n::text("settings.security.trusted_devices"));
        ui.horizontal(|ui| {
            let trusted = settings.trusted_admin_endpoints.len();
            ui.label(
                RichText::new(if trusted == 0 {
                    i18n::text("settings.security.trusted_none").to_string()
                } else {
                    i18n::plural("settings.security.trusted_count", trusted as u64, &[])
                })
                .size(11.0)
                .color(if trusted > 0 { TEXT } else { TEXT_DIM }),
            );
            if ui
                .add_enabled(
                    trusted > 0,
                    egui::Button::new(i18n::text("settings.security.forget_all")),
                )
                .on_hover_text(i18n::text("settings.security.forget_all_hint"))
                .clicked()
            {
                settings.trusted_admin_endpoints.clear();
            }
        });
        ui.end_row();
    });

    if settings.default_allow_mouse || settings.default_allow_keyboard {
        ui.add_space(4.0);
        ui.label(
            RichText::new(i18n::text("settings.security.pre_ticked_warning"))
                .size(11.0)
                .color(AMBER),
        );
    }
}

/// This application's own appearance, as opposed to what it shows of another
/// machine.
fn interface_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    use super::app::SCALE_EPSILON as EPSILON;

    heading(ui, i18n::text("settings.interface"));

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
        ui.label(i18n::text("settings.interface.scale"))
            .on_hover_text(i18n::text("settings.interface.scale_hint"));
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
                .add_enabled(
                    off_default,
                    egui::Button::new(i18n::text("settings.interface.hundred")),
                )
                .on_hover_text(i18n::text("settings.interface.follow_monitor"))
                .clicked()
            {
                *pending = 1.0;
            }

            let differs = (*pending - settings.ui_scale).abs() > EPSILON;
            if ui
                .add_enabled(
                    differs,
                    egui::Button::new(i18n::text("settings.interface.apply")),
                )
                .on_hover_text(i18n::text("settings.interface.apply_hint"))
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
        i18n::format(
            "settings.interface.currently",
            &[("percent", &format!("{:.0}%", settings.ui_scale * 100.0))],
        )
    } else {
        i18n::text("settings.interface.apply_note").to_string()
    };
    ui.label(RichText::new(note).size(11.0).color(TEXT_DIM));
}

/// What this machine does with a session it connects **out** to.
///
/// The other half of the split described on [`video_section`]. Nothing here
/// reaches the host: it is all about how the picture arriving is decoded and
/// drawn on this screen.
fn viewing_section(ui: &mut egui::Ui, settings: &mut Settings) {
    heading(ui, i18n::text("settings.viewing"));

    egui::Grid::new("viewing").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(i18n::text("settings.viewing.scaling"))
            .on_hover_text(i18n::text("settings.viewing.scaling_hint"));
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
            ui.label(i18n::text("settings.viewing.gpu_present"))
                .on_hover_text(i18n::text("settings.viewing.gpu_present_hint"));
            ui.checkbox(&mut settings.gpu_present, i18n::text("settings.viewing.skip_readback"));
            ui.end_row();
        }

        ui.label("");
        ui.checkbox(
            &mut settings.fullscreen_on_connect,
            i18n::text("settings.viewing.fullscreen_on_connect"),
        );
        ui.end_row();
    });
}

fn shortcut_section(ui: &mut egui::Ui, settings: &mut Settings, panel: &mut PanelState) {
    heading(ui, i18n::text("settings.shortcuts"));
    ui.label(
        RichText::new(i18n::text("settings.shortcuts.blurb"))
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
            let text = if capturing {
                i18n::text("settings.shortcuts.press_a_key").to_string()
            } else {
                shortcut.to_string()
            };

            if ui
                .add_sized(
                    [140.0, 22.0],
                    egui::SelectableLabel::new(capturing, text),
                )
                .on_hover_text(i18n::text("settings.shortcuts.rebind_hint"))
                .clicked()
            {
                panel.capturing = if capturing { None } else { Some(action) };
            }

            // Warn rather than block: refusing the first half of a swap is more
            // annoying than flagging the overlap.
            if conflicts.contains(&action) {
                ui.label(
                    RichText::new(i18n::text("settings.shortcuts.duplicate"))
                        .size(11.0)
                        .color(AMBER),
                );
            } else if !shortcut.is_safe() {
                ui.label(
                    RichText::new(i18n::text("settings.shortcuts.may_block"))
                        .size(11.0)
                        .color(AMBER),
                )
                .on_hover_text(i18n::text("settings.shortcuts.may_block_hint"));
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

        // By the window's own id rather than one derived from its title: the
        // title is translated now, and a test that looked it up by its English
        // words would pass in English and fail in every other language.
        let rect = ctx
            .memory(|m| m.area_rect(egui::Id::new("settings-window")))
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
