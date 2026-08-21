//! Translate egui keyboard input into protocol key events.
//!
//! The wire protocol always carries **Windows virtual-key codes**, so this is
//! where egui's platform-neutral `Key` becomes a VK.
//!
//! ## Modifiers are synthesized
//!
//! egui's `Key` enum has no Shift/Ctrl/Alt variants — modifier state arrives as a
//! separate `Modifiers` struct that describes the *current* state, not
//! transitions. A remote host needs real key-down and key-up events for
//! modifiers (otherwise Ctrl+C on the guest sends a bare `C`), so
//! [`ModifierTracker`] diffs that state each frame and emits the missing events.
//!
//! ## Known gap: the Windows/Super key
//!
//! egui does not report it at all. It cannot be forwarded, and in practice the
//! guest's own OS swallows most Win combinations before egui sees them.

use egui::{Key, Modifiers};
use tain_core::protocol::{modifier, KeyEvent};

// Virtual-key codes we reference by name.
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21; // PageUp
const VK_NEXT: u16 = 0x22; // PageDown
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2D;
const VK_DELETE: u16 = 0x2E;
const VK_OEM_1: u16 = 0xBA; // ; :
const VK_OEM_PLUS: u16 = 0xBB; // = +
const VK_OEM_COMMA: u16 = 0xBC; // , <
const VK_OEM_MINUS: u16 = 0xBD; // - _
const VK_OEM_PERIOD: u16 = 0xBE; // . >
const VK_OEM_2: u16 = 0xBF; // / ?
const VK_OEM_3: u16 = 0xC0; // ` ~
const VK_OEM_4: u16 = 0xDB; // [ {
const VK_OEM_5: u16 = 0xDC; // \ |
const VK_OEM_6: u16 = 0xDD; // ] }
const VK_OEM_7: u16 = 0xDE; // ' "

/// Map an egui key to its Windows virtual-key code.
///
/// Returns `None` for keys with no physical equivalent — egui's `Copy`/`Cut`/
/// `Paste` are synthesized from Ctrl combinations rather than real keys, and the
/// function keys above F24 do not exist in the VK space.
pub fn vk_for(key: Key) -> Option<u16> {
    let vk = match key {
        Key::ArrowDown => VK_DOWN,
        Key::ArrowLeft => VK_LEFT,
        Key::ArrowRight => VK_RIGHT,
        Key::ArrowUp => VK_UP,

        Key::Escape => VK_ESCAPE,
        Key::Tab => VK_TAB,
        Key::Backspace => VK_BACK,
        Key::Enter => VK_RETURN,
        Key::Space => VK_SPACE,

        Key::Insert => VK_INSERT,
        Key::Delete => VK_DELETE,
        Key::Home => VK_HOME,
        Key::End => VK_END,
        Key::PageUp => VK_PRIOR,
        Key::PageDown => VK_NEXT,

        // Not physical keys — egui produces these from Ctrl+C/X/V, which we
        // already forward as their component key events.
        Key::Copy | Key::Cut | Key::Paste => return None,

        // Punctuation maps to the *physical* key; the shifted variant is
        // reproduced on the host by the accompanying Shift key event.
        Key::Colon | Key::Semicolon => VK_OEM_1,
        Key::Plus | Key::Equals => VK_OEM_PLUS,
        Key::Comma => VK_OEM_COMMA,
        Key::Minus => VK_OEM_MINUS,
        Key::Period => VK_OEM_PERIOD,
        Key::Slash | Key::Questionmark => VK_OEM_2,
        Key::Backtick => VK_OEM_3,
        Key::OpenBracket => VK_OEM_4,
        Key::Backslash | Key::Pipe => VK_OEM_5,
        Key::CloseBracket => VK_OEM_6,
        Key::Quote => VK_OEM_7,

        // Digits and letters are contiguous in the VK space and match ASCII.
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,

        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,

        // VK_F1 is 0x70 and the function keys run contiguously to F24 (0x87).
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::F13 => 0x7C,
        Key::F14 => 0x7D,
        Key::F15 => 0x7E,
        Key::F16 => 0x7F,
        Key::F17 => 0x80,
        Key::F18 => 0x81,
        Key::F19 => 0x82,
        Key::F20 => 0x83,
        Key::F21 => 0x84,
        Key::F22 => 0x85,
        Key::F23 => 0x86,
        Key::F24 => 0x87,

        // F25 and above have no virtual-key code.
        _ => return None,
    };
    Some(vk)
}

/// Pack egui modifier state into the protocol's advisory bitmask.
pub fn modifier_bits(m: Modifiers) -> u8 {
    let mut bits = 0;
    if m.ctrl {
        bits |= modifier::CTRL;
    }
    if m.alt {
        bits |= modifier::ALT;
    }
    if m.shift {
        bits |= modifier::SHIFT;
    }
    bits
}

/// The six modifier keys, matching `immersive::MODIFIER_VKS`.
const MODIFIER_VKS_LOCAL: [u16; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];

/// Emits key events for modifiers, tracking what is really held.
#[derive(Default)]
pub struct ModifierTracker {
    /// Per-side state from the operating system, for [`Self::diff_actual`].
    actual: [bool; 6],
}

impl ModifierTracker {
    /// Emit events for modifiers whose real state has changed.
    ///
    /// Takes what the operating system says is held, one flag per side, rather
    /// than egui's combined view — see `immersive::key_is_down`. Left and right
    /// are separate keys to the host, so they are separate here too.
    pub fn diff_actual(&mut self, down: [bool; 6], vks: [u16; 6]) -> Vec<KeyEvent> {
        let mut events = Vec::new();
        for (index, (&is, &vk)) in down.iter().zip(vks.iter()).enumerate() {
            if self.actual[index] == is {
                continue;
            }
            self.actual[index] = is;
            events.push(KeyEvent {
                keycode: vk,
                pressed: is,
                modifiers: 0,
                // No scancode: this is read state, not an observed keystroke.
                scancode: 0,
                extended: false,
            });
        }
        events
    }

    /// Release every modifier we believe is held.
    ///
    /// Called when the remote view loses focus or the session ends — otherwise a
    /// modifier held at that moment stays stuck down on the host forever.
    pub fn release_all(&mut self) -> Vec<KeyEvent> {
        let mut events = Vec::new();
        // Whatever the OS last said was held, let go of — on the host, which
        // cannot see that this end has stopped forwarding.
        for (index, &vk) in MODIFIER_VKS_LOCAL.iter().enumerate() {
            if !std::mem::replace(&mut self.actual[index], false) {
                continue;
            }
            events.push(KeyEvent {
                keycode: vk,
                pressed: false,
                modifiers: 0,
                scancode: 0,
                extended: false,
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_match_their_ascii_codes() {
        assert_eq!(vk_for(Key::A), Some(b'A' as u16));
        assert_eq!(vk_for(Key::Z), Some(b'Z' as u16));
        assert_eq!(vk_for(Key::Num0), Some(b'0' as u16));
        assert_eq!(vk_for(Key::Num9), Some(b'9' as u16));
    }

    #[test]
    fn function_keys_are_contiguous_from_f1() {
        assert_eq!(vk_for(Key::F1), Some(0x70));
        assert_eq!(vk_for(Key::F12), Some(0x7B));
        assert_eq!(vk_for(Key::F24), Some(0x87));
        // Above F24 there is no VK code to send.
        assert_eq!(vk_for(Key::F25), None);
        assert_eq!(vk_for(Key::F35), None);
    }

    #[test]
    fn navigation_keys_map_to_their_extended_vks() {
        assert_eq!(vk_for(Key::ArrowUp), Some(VK_UP));
        assert_eq!(vk_for(Key::ArrowDown), Some(VK_DOWN));
        assert_eq!(vk_for(Key::Home), Some(VK_HOME));
        assert_eq!(vk_for(Key::PageDown), Some(VK_NEXT));
        assert_eq!(vk_for(Key::Delete), Some(VK_DELETE));
    }

    #[test]
    fn shifted_punctuation_shares_its_physical_key() {
        // The host reconstructs the shifted character from the Shift key event,
        // so both egui variants must resolve to the same physical VK.
        assert_eq!(vk_for(Key::Semicolon), vk_for(Key::Colon));
        assert_eq!(vk_for(Key::Slash), vk_for(Key::Questionmark));
        assert_eq!(vk_for(Key::Backslash), vk_for(Key::Pipe));
        assert_eq!(vk_for(Key::Equals), vk_for(Key::Plus));
    }

    #[test]
    fn synthetic_clipboard_keys_are_not_forwarded() {
        assert_eq!(vk_for(Key::Copy), None);
        assert_eq!(vk_for(Key::Cut), None);
        assert_eq!(vk_for(Key::Paste), None);
    }

    #[test]
    fn every_mapped_key_is_a_valid_virtual_key() {
        // VK codes are a single byte; anything larger would be a typo.
        for key in Key::ALL {
            if let Some(vk) = vk_for(*key) {
                assert!(vk > 0 && vk <= 0xFF, "{key:?} produced out-of-range VK {vk:#x}");
            }
        }
    }

    #[test]
    fn distinct_keys_do_not_collide_unless_they_share_a_physical_key() {
        use std::collections::HashMap;
        // Groups that legitimately share one physical key.
        let shared: &[&[Key]] = &[
            &[Key::Semicolon, Key::Colon],
            &[Key::Equals, Key::Plus],
            &[Key::Slash, Key::Questionmark],
            &[Key::Backslash, Key::Pipe],
        ];
        let allowed = |a: Key, b: Key| {
            shared.iter().any(|g| g.contains(&a) && g.contains(&b))
        };

        let mut seen: HashMap<u16, Key> = HashMap::new();
        for key in Key::ALL {
            let Some(vk) = vk_for(*key) else { continue };
            if let Some(prev) = seen.insert(vk, *key) {
                assert!(allowed(prev, *key), "{prev:?} and {key:?} both map to {vk:#x}");
            }
        }
    }

    fn mods(ctrl: bool, alt: bool, shift: bool) -> Modifiers {
        Modifiers { ctrl, alt, shift, ..Default::default() }
    }

    /// The six modifier VKs, in the order `diff_actual` expects.
    const VKS: [u16; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];

    /// Only transitions produce events, and each side is its own key.
    ///
    /// # The bug this is here for
    ///
    /// Modifiers were derived from egui's combined `Modifiers` flag, which is
    /// unreliable inside an immediate viewport and cannot express left versus
    /// right at all. Ctrl and Alt never reached the host as a result. State now
    /// comes from the operating system, one flag per physical key.
    #[test]
    fn only_real_changes_are_sent_and_the_sides_are_distinct() {
        let mut tracker = ModifierTracker::default();
        let none = [false; 6];

        // Left Ctrl down.
        let mut down = none;
        down[2] = true;
        let events = tracker.diff_actual(down, VKS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].keycode, 0xA2);
        assert!(events[0].pressed);

        // Unchanged state says nothing, or the wire fills with repeats.
        assert!(tracker.diff_actual(down, VKS).is_empty());

        // Right Ctrl is a different key, not the same one again.
        down[3] = true;
        let events = tracker.diff_actual(down, VKS);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].keycode, 0xA3);

        let up = tracker.diff_actual(none, VKS);
        assert_eq!(up.len(), 2, "both sides have to be let go");
        assert!(up.iter().all(|e| !e.pressed));
    }

    /// Anything held when forwarding stops must be released on the host.
    ///
    /// The host cannot see that this end stopped sending, so a modifier left
    /// down there changes the meaning of everything typed afterwards.
    #[test]
    fn release_all_clears_held_modifiers_exactly_once() {
        let mut tracker = ModifierTracker::default();
        let mut down = [false; 6];
        down[2] = true;
        down[4] = true;
        tracker.diff_actual(down, VKS);

        let released = tracker.release_all();
        assert_eq!(released.len(), 2);
        assert!(released.iter().all(|e| !e.pressed));

        assert!(tracker.release_all().is_empty(), "second call must be a no-op");
    }

    #[test]
    fn modifier_bits_pack_as_documented() {
        assert_eq!(modifier_bits(mods(false, false, false)), 0);
        assert_eq!(modifier_bits(mods(true, false, false)), modifier::CTRL);
        assert_eq!(modifier_bits(mods(false, true, false)), modifier::ALT);
        assert_eq!(modifier_bits(mods(false, false, true)), modifier::SHIFT);
        assert_eq!(
            modifier_bits(mods(true, true, true)),
            modifier::CTRL | modifier::ALT | modifier::SHIFT
        );
    }
}
