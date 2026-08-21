//! User-configurable keyboard shortcuts for session control.
//!
//! These are handled **locally and never forwarded** to the host, so they must
//! be distinguishable from ordinary typing. A bare letter would be swallowed
//! mid-sentence in a remote text editor, which is why [`Shortcut::is_safe`]
//! requires a modifier for anything that isn't a function or navigation key.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Something a shortcut can trigger during a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShortcutAction {
    Disconnect,
    ToggleFullscreen,
    ToggleToolbar,
    ToggleImmersive,
}

impl ShortcutAction {
    pub const ALL: [Self; 4] = [
        Self::Disconnect,
        Self::ToggleFullscreen,
        Self::ToggleToolbar,
        Self::ToggleImmersive,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnect => "Disconnect",
            Self::ToggleFullscreen => "Toggle fullscreen",
            Self::ToggleToolbar => "Toggle toolbar",
            Self::ToggleImmersive => "Toggle immersive mode",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Disconnect => "End the current session and return to the connection list",
            Self::ToggleFullscreen => "Switch the window between fullscreen and windowed",
            Self::ToggleToolbar => "Show or hide the session controls overlay",
            Self::ToggleImmersive => {
                "Send every key to the host, including Win and Alt, and keep the pointer inside the window"
            }
        }
    }

    fn default_binding(self) -> Shortcut {
        // All four share Ctrl+Shift on purpose. They are consumed locally and
        // never forwarded, so each one is a combination the host can never
        // receive — keeping them on one modifier pair makes that cost explicit
        // and memorable, instead of scattering holes through the keymap.
        let with_ctrl_shift = |key| Shortcut::new(key, true, true, false);
        match self {
            Self::Disconnect => with_ctrl_shift(egui::Key::Q),
            Self::ToggleFullscreen => with_ctrl_shift(egui::Key::W),
            Self::ToggleToolbar => with_ctrl_shift(egui::Key::A),
            Self::ToggleImmersive => with_ctrl_shift(egui::Key::E),
        }
    }
}

/// A key plus modifiers.
///
/// `egui::Key` is not serializable, so bindings persist **by name** rather than
/// by index — an egui upgrade that reorders the enum must not silently rebind
/// every shortcut. That costs `Copy`, which is a fair trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    /// egui key name, e.g. `"F11"` or `"D"`.
    pub key: KeyName,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Shortcut {
    pub fn new(key: egui::Key, ctrl: bool, shift: bool, alt: bool) -> Self {
        Self { key: KeyName(key.name().to_string()), ctrl, shift, alt }
    }

    /// The bound key, or `None` if the stored name is not one egui knows.
    pub fn egui_key(&self) -> Option<egui::Key> {
        egui::Key::ALL.iter().copied().find(|k| k.name() == self.key.0)
    }

    /// Does this frame's input match the binding?
    ///
    /// Modifiers must match exactly — otherwise `Ctrl+Shift+D` would also fire
    /// on `Ctrl+D`, and a shortcut would eat combinations meant for the host.
    pub fn matches(&self, input: &egui::InputState) -> bool {
        let Some(key) = self.egui_key() else { return false };
        input.key_pressed(key)
            && input.modifiers.ctrl == self.ctrl
            && input.modifiers.shift == self.shift
            && input.modifiers.alt == self.alt
    }

    /// Whether this binding is distinguishable from ordinary typing.
    ///
    /// Function and navigation keys stand alone; anything else needs a modifier
    /// or it would be stolen from the host mid-sentence.
    pub fn is_safe(&self) -> bool {
        if self.ctrl || self.alt {
            return true;
        }
        let Some(key) = self.egui_key() else { return false };
        matches!(
            key,
            egui::Key::F1
                | egui::Key::F2
                | egui::Key::F3
                | egui::Key::F4
                | egui::Key::F5
                | egui::Key::F6
                | egui::Key::F7
                | egui::Key::F8
                | egui::Key::F9
                | egui::Key::F10
                | egui::Key::F11
                | egui::Key::F12
                | egui::Key::Insert
                | egui::Key::Home
                | egui::Key::End
                | egui::Key::PageUp
                | egui::Key::PageDown
        )
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        f.write_str(&self.key.0)
    }
}

/// Newtype so a stored key name stays a string in JSON without leaking `String`
/// semantics into comparisons elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyName(pub String);

/// The full set of bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShortcutBindings {
    pub disconnect: Shortcut,
    pub toggle_fullscreen: Shortcut,
    pub toggle_toolbar: Shortcut,
    pub toggle_immersive: Shortcut,
}

impl Default for ShortcutBindings {
    fn default() -> Self {
        Self {
            disconnect: ShortcutAction::Disconnect.default_binding(),
            toggle_fullscreen: ShortcutAction::ToggleFullscreen.default_binding(),
            toggle_toolbar: ShortcutAction::ToggleToolbar.default_binding(),
            toggle_immersive: ShortcutAction::ToggleImmersive.default_binding(),
        }
    }
}

impl ShortcutBindings {
    pub fn get(&self, action: ShortcutAction) -> &Shortcut {
        match action {
            ShortcutAction::Disconnect => &self.disconnect,
            ShortcutAction::ToggleFullscreen => &self.toggle_fullscreen,
            ShortcutAction::ToggleToolbar => &self.toggle_toolbar,
            ShortcutAction::ToggleImmersive => &self.toggle_immersive,
        }
    }

    pub fn set(&mut self, action: ShortcutAction, shortcut: Shortcut) {
        match action {
            ShortcutAction::Disconnect => self.disconnect = shortcut,
            ShortcutAction::ToggleFullscreen => self.toggle_fullscreen = shortcut,
            ShortcutAction::ToggleToolbar => self.toggle_toolbar = shortcut,
            ShortcutAction::ToggleImmersive => self.toggle_immersive = shortcut,
        }
    }

    /// Actions bound to the same key combination as something else.
    ///
    /// Surfaced rather than prevented — the user may be mid-rebind, and refusing
    /// the first half of a swap is more annoying than flagging it.
    pub fn conflicts(&self) -> Vec<ShortcutAction> {
        let mut conflicting = Vec::new();
        for (i, a) in ShortcutAction::ALL.iter().enumerate() {
            for b in &ShortcutAction::ALL[i + 1..] {
                if self.get(*a) == self.get(*b) {
                    conflicting.push(*a);
                    conflicting.push(*b);
                }
            }
        }
        conflicting.dedup();
        conflicting
    }

    /// Which action, if any, this frame's input triggers.
    pub fn triggered(&self, input: &egui::InputState) -> Option<ShortcutAction> {
        ShortcutAction::ALL.into_iter().find(|a| self.get(*a).matches(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;

    #[test]
    fn defaults_are_all_safe_and_distinct() {
        let bindings = ShortcutBindings::default();
        for action in ShortcutAction::ALL {
            assert!(
                bindings.get(action).is_safe(),
                "{action:?} default is indistinguishable from typing"
            );
        }
        assert!(bindings.conflicts().is_empty(), "defaults must not collide");
    }

    #[test]
    fn defaults_are_the_documented_ctrl_shift_set() {
        let bindings = ShortcutBindings::default();
        assert_eq!(bindings.disconnect.to_string(), "Ctrl+Shift+Q");
        assert_eq!(bindings.toggle_fullscreen.to_string(), "Ctrl+Shift+W");
        assert_eq!(bindings.toggle_toolbar.to_string(), "Ctrl+Shift+A");
        assert_eq!(bindings.toggle_immersive.to_string(), "Ctrl+Shift+E");
    }

    #[test]
    fn defaults_all_carry_a_modifier_the_host_can_never_see() {
        // Every default is consumed locally, so each one is a combination the
        // host loses. Keeping them on one modifier pair bounds that cost.
        let bindings = ShortcutBindings::default();
        for action in ShortcutAction::ALL {
            let shortcut = bindings.get(action);
            assert!(shortcut.ctrl && shortcut.shift, "{action:?} should be Ctrl+Shift");
            assert!(!shortcut.alt, "{action:?} should not need Alt as well");
        }
    }

    #[test]
    fn bindings_round_trip_through_json() {
        let bindings = ShortcutBindings::default();
        let text = serde_json::to_string(&bindings).unwrap();
        assert_eq!(serde_json::from_str::<ShortcutBindings>(&text).unwrap(), bindings);
    }

    #[test]
    fn a_stored_key_resolves_back_to_the_egui_key() {
        assert_eq!(Shortcut::new(Key::F11, false, false, false).egui_key(), Some(Key::F11));
        assert_eq!(Shortcut::new(Key::D, true, true, false).egui_key(), Some(Key::D));
    }

    #[test]
    fn an_unknown_key_name_never_matches_and_is_not_safe() {
        // A config file naming a key this egui build doesn't have must not panic
        // or silently bind to something else.
        let bogus = Shortcut { key: KeyName("Nonexistent".into()), ctrl: true, shift: false, alt: false };
        assert_eq!(bogus.egui_key(), None);
        // ctrl alone still reads as "safe"; the key itself simply never fires.
        assert!(bogus.is_safe());

        let bare = Shortcut { key: KeyName("Nonexistent".into()), ctrl: false, shift: false, alt: false };
        assert!(!bare.is_safe());
    }

    #[test]
    fn bare_letters_are_rejected_as_unsafe() {
        // These would be stolen from the host mid-sentence.
        assert!(!Shortcut::new(Key::D, false, false, false).is_safe());
        assert!(!Shortcut::new(Key::Escape, false, false, false).is_safe());
        assert!(!Shortcut::new(Key::D, false, true, false).is_safe(), "shift alone is typing");

        assert!(Shortcut::new(Key::D, true, false, false).is_safe());
        assert!(Shortcut::new(Key::D, false, false, true).is_safe());
    }

    #[test]
    fn function_and_navigation_keys_stand_alone() {
        for key in [Key::F1, Key::F9, Key::F11, Key::F12, Key::Home, Key::PageUp] {
            assert!(Shortcut::new(key, false, false, false).is_safe(), "{key:?} should be usable bare");
        }
    }

    #[test]
    fn display_reads_as_a_conventional_shortcut() {
        assert_eq!(Shortcut::new(Key::F11, false, false, false).to_string(), "F11");
        assert_eq!(Shortcut::new(Key::D, true, true, false).to_string(), "Ctrl+Shift+D");
        assert_eq!(Shortcut::new(Key::A, false, false, true).to_string(), "Alt+A");
    }

    #[test]
    fn conflicts_are_reported_for_duplicate_bindings() {
        let mut bindings = ShortcutBindings::default();
        assert!(bindings.conflicts().is_empty());

        bindings.set(ShortcutAction::ToggleToolbar, bindings.get(ShortcutAction::ToggleFullscreen).clone());
        let conflicts = bindings.conflicts();
        assert!(conflicts.contains(&ShortcutAction::ToggleToolbar));
        assert!(conflicts.contains(&ShortcutAction::ToggleFullscreen));
        assert!(!conflicts.contains(&ShortcutAction::Disconnect));
    }

    #[test]
    fn set_and_get_round_trip_for_every_action() {
        let mut bindings = ShortcutBindings::default();
        for action in ShortcutAction::ALL {
            let custom = Shortcut::new(Key::F7, true, false, true);
            bindings.set(action, custom.clone());
            assert_eq!(*bindings.get(action), custom);
        }
    }

    #[test]
    fn every_action_is_labelled_and_described() {
        for action in ShortcutAction::ALL {
            assert!(!action.label().is_empty());
            assert!(!action.description().is_empty());
        }
    }
}
