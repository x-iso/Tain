//! The reminder that comes up while Ctrl or Shift is held in a session.
//!
//! # Why it exists
//!
//! Tain's shortcuts are combinations the host never receives, and each one is a
//! chord somebody may well be partway through for another reason entirely —
//! Ctrl and Shift held to extend a selection, Shift held to crouch in a game.
//! Nothing said that a Tain shortcut was one key away, or which one, so the
//! first anyone heard of a binding was the session doing something they had
//! not asked for. And nothing reminded the people who did want them what the
//! keys were.
//!
//! So after a second of holding Ctrl or Shift, a panel in the picture's lower
//! left lists the shortcuts: the modifiers already down are lit, the key that
//! would fire one is picked out once nothing else is missing, and those that
//! cannot fire — a modifier held that they do not use, or in immersive mode one
//! that goes to the host — are faded.

use egui::{Align2, Color32, Context, Id, Order, Pos2, RichText, Stroke};

use tain_core::t;

use super::immersive::{MOD_ALT, MOD_CTRL, MOD_SHIFT};
use super::shortcuts::{Shortcut, ShortcutAction, ShortcutBindings};
use super::theme::*;

/// How long Ctrl or Shift has to be held before the reminder shows, in seconds.
///
/// Long enough that a shortcut pressed by somebody who knows it never brings it
/// up, short enough to arrive while a hand is still hovering.
const HOLD: f64 = 1.0;

#[derive(Default)]
pub struct ShortcutHint {
    /// When the current hold began, in egui's seconds.
    held_since: Option<f64>,
    /// A shortcut fired during this hold. The reminder has done its job and
    /// stays away until the modifiers are let go.
    used: bool,
}

impl ShortcutHint {
    /// A shortcut has just fired.
    pub fn used(&mut self) {
        self.used = true;
    }

    /// Show the reminder, if Ctrl or Shift has been held long enough.
    ///
    /// `held` is what is held, as `MOD_*` bits; `corner` is the bottom-left of
    /// the picture; `in_front` whether the session window has the keyboard at
    /// all, since a modifier held in another application is none of its
    /// business.
    pub fn show(
        &mut self,
        ctx: &Context,
        corner: Pos2,
        bindings: &ShortcutBindings,
        held: u8,
        immersed: bool,
        in_front: bool,
    ) {
        if !in_front || held & (MOD_CTRL | MOD_SHIFT) == 0 {
            self.held_since = None;
            self.used = false;
            return;
        }
        let now = ctx.input(|i| i.time);
        let since = *self.held_since.get_or_insert(now);
        // Often enough to come up on time and to follow the modifiers as they
        // change, which no event may announce: in immersive mode the keys never
        // reach egui at all. Only while one is held.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        if self.used || now - since < HOLD {
            return;
        }

        egui::Area::new(Id::new("shortcut-hint"))
            .order(Order::Foreground)
            // Over the picture, and never in the way of it: a press goes to the
            // host as though this were not here.
            .interactable(false)
            .pivot(Align2::LEFT_BOTTOM)
            .fixed_pos(corner + egui::vec2(16.0, -16.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_black_alpha(225))
                    .rounding(6.0)
                    .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(t!("shortcut.hint_title")).size(11.0).color(TEXT_DIM));
                        ui.add_space(4.0);
                        for action in ShortcutAction::ALL {
                            row(ui, action, bindings.get(action), held, immersed);
                        }
                    });
            });
    }
}

/// Where a shortcut stands, given what is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// Everything it needs is held and nothing else: its key fires it.
    Ready,
    /// Some of what it needs is held.
    Partway,
    /// Something is held that it does not use, so it cannot fire.
    Blocked,
    /// Immersive mode is on and this one goes to the host.
    ToHost,
}

fn standing(action: ShortcutAction, binding: &Shortcut, held: u8, immersed: bool) -> Standing {
    let needs = needs(binding);
    if immersed && action != ShortcutAction::ToggleImmersive && !binding.in_immersive {
        Standing::ToHost
    } else if held & !needs != 0 {
        Standing::Blocked
    } else if held == needs {
        Standing::Ready
    } else {
        Standing::Partway
    }
}

fn needs(binding: &Shortcut) -> u8 {
    let mut bits = 0;
    if binding.ctrl {
        bits |= MOD_CTRL;
    }
    if binding.shift {
        bits |= MOD_SHIFT;
    }
    if binding.alt {
        bits |= MOD_ALT;
    }
    bits
}

fn row(ui: &mut egui::Ui, action: ShortcutAction, binding: &Shortcut, held: u8, immersed: bool) {
    let standing = standing(action, binding, held, immersed);
    ui.horizontal(|ui| {
        if matches!(standing, Standing::Blocked | Standing::ToHost) {
            ui.multiply_opacity(0.4);
        }
        ui.spacing_mut().item_spacing.x = 4.0;
        for (wanted, bit, name) in [
            (binding.ctrl, MOD_CTRL, "Ctrl"),
            (binding.shift, MOD_SHIFT, "Shift"),
            (binding.alt, MOD_ALT, "Alt"),
        ] {
            if wanted {
                chip(ui, name, if held & bit != 0 { Chip::Held } else { Chip::Waiting });
            }
        }
        chip(ui, &binding.key.0, if standing == Standing::Ready { Chip::Next } else { Chip::Waiting });
        ui.add_space(6.0);
        let ink = if standing == Standing::Ready { TEXT } else { TEXT_DIM };
        ui.label(RichText::new(action.label()).size(12.0).color(ink));
        if standing == Standing::ToHost {
            ui.label(RichText::new(t!("shortcut.hint_to_host")).size(11.0).color(TEXT_DIM));
        }
    });
}

enum Chip {
    /// A modifier that is held.
    Held,
    /// Not pressed yet.
    Waiting,
    /// The key that would fire it now.
    Next,
}

fn chip(ui: &mut egui::Ui, text: &str, chip: Chip) {
    let (fill, stroke, ink) = match chip {
        Chip::Held => (SELECT_BLUE, Stroke::NONE, Color32::WHITE),
        Chip::Next => (TEXT, Stroke::NONE, Color32::BLACK),
        Chip::Waiting => {
            (Color32::TRANSPARENT, Stroke::new(1.0, Color32::from_white_alpha(70)), TEXT_DIM)
        }
    };
    egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(4.0)
        .inner_margin(egui::Margin::symmetric(5.0, 1.0))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.0).color(ink)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl_shift_w() -> Shortcut {
        Shortcut::new(egui::Key::W, true, true, false)
    }

    #[test]
    fn its_key_fires_it_once_everything_it_needs_is_held() {
        let binding = ctrl_shift_w();
        let fullscreen = ShortcutAction::ToggleFullscreen;
        assert_eq!(standing(fullscreen, &binding, MOD_CTRL, false), Standing::Partway);
        assert_eq!(standing(fullscreen, &binding, MOD_CTRL | MOD_SHIFT, false), Standing::Ready);
    }

    #[test]
    fn a_modifier_it_does_not_use_stops_it_firing() {
        // Shortcuts match their modifiers exactly, so Ctrl+Shift+Alt+W is not
        // Ctrl+Shift+W, and the reminder must not say it is.
        let held = MOD_CTRL | MOD_SHIFT | MOD_ALT;
        assert_eq!(
            standing(ShortcutAction::ToggleFullscreen, &ctrl_shift_w(), held, false),
            Standing::Blocked
        );
    }

    #[test]
    fn immersed_the_ones_left_to_the_host_say_so_and_the_way_out_never_does() {
        let mut binding = ctrl_shift_w();
        binding.in_immersive = false;
        let held = MOD_CTRL | MOD_SHIFT;
        assert_eq!(standing(ShortcutAction::ToggleFullscreen, &binding, held, true), Standing::ToHost);
        assert_eq!(standing(ShortcutAction::ToggleFullscreen, &binding, held, false), Standing::Ready);
        assert_eq!(standing(ShortcutAction::ToggleImmersive, &binding, held, true), Standing::Ready);
    }
}
