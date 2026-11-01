//! Input injection on Linux, through the RemoteDesktop portal.
//!
//! The same portal session that shares the screen ([`crate::portal`]) takes
//! the guest's pointer, keyboard and touch: the person at the host granted
//! both in one dialog. Events go to the desktop as D-Bus calls, without
//! waiting for replies, so they cost a message each rather than a round trip.
//!
//! ## Coordinates
//!
//! Absolute positions are given in the shared stream's *logical* size — what
//! the desktop calls the screen's size, which on a scaled display is smaller
//! than the frames — so a guest's normalized `0..=65535` maps through that.
//!
//! ## Keys
//!
//! The desktop wants Linux key codes, which name positions on the keyboard,
//! like a scancode. A Windows guest sends its scancode, which maps to one
//! directly; a guest without one sends only a Windows virtual-key code, which
//! is mapped by what the key is. Either way the host's own keyboard layout
//! turns the position into a character, as it does for a local keyboard.
//!
//! ## No session, no input
//!
//! Input is never what opens a portal session: that would put the sharing
//! dialog up for a guest who is not even watching yet. Until a capture has
//! opened one, events are dropped, and said so once.

#![cfg(target_os = "linux")]

use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;
use zbus::zvariant::Value;

use crate::input_inject::{
    InputInjector, KeyInput, Mapping, MouseButtonInput, MousePosition, ScrollInput, Shown, TouchInput,
};
use crate::portal::{self, RemoteSession, DEVICE_KEYBOARD, DEVICE_POINTER, DEVICE_TOUCHSCREEN};
use crate::protocol::{TouchPhase, WHEEL_DELTA};

/// Linux's `BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`.
const BUTTONS: [i32; 5] = [0x110, 0x111, 0x112, 0x113, 0x114];

/// Discrete axes, as the portal numbers them.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

/// The RemoteDesktop portal, as an [`InputInjector`].
#[derive(Default)]
pub struct PortalInjector {
    /// What the guest is shown, for when the portal does not say the stream's
    /// logical size.
    shown: Mutex<Option<Shown>>,
    /// Wheel movement not yet a whole detent, per axis, so a touchpad's small
    /// deltas add up instead of being lost.
    scroll_remainder: Mutex<(i32, i32)>,
    /// Touch contacts down, by the guest's id, as the portal's slots.
    touches: Mutex<HashMap<u8, u32>>,
    warned_no_session: AtomicBool,
    warned_device: AtomicBool,
}

impl PortalInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The portal session, if a capture has opened one and it granted `device`.
    fn session(&self, device: u32) -> Option<Arc<RemoteSession>> {
        let Some(session) = portal::current() else {
            if !self.warned_no_session.swap(true, Ordering::Relaxed) {
                info!("No screen is being shared yet, so there is nowhere to send input");
            }
            return None;
        };
        self.warned_no_session.store(false, Ordering::Relaxed);
        if session.devices() & device == 0 {
            if !self.warned_device.swap(true, Ordering::Relaxed) {
                warn!(
                    "The desktop did not grant {} — that input is dropped",
                    match device {
                        DEVICE_KEYBOARD => "the keyboard",
                        DEVICE_POINTER => "the pointer",
                        _ => "touch",
                    }
                );
            }
            return None;
        }
        Some(session)
    }

    /// Where a normalized position lands in the stream's logical coordinates,
    /// and the stream it belongs to.
    fn place(&self, session: &RemoteSession, x: u16, y: u16) -> Option<(u32, f64, f64)> {
        let stream = session.streams().first()?;
        let (width, height) = match stream.size {
            Some((w, h)) if w > 0 && h > 0 => (w as f64, h as f64),
            _ => {
                let shown = (*self.shown.lock().unwrap_or_else(|e| e.into_inner()))?;
                (shown.width as f64, shown.height as f64)
            }
        };
        Some((stream.node_id, x as f64 / 65535.0 * width, y as f64 / 65535.0 * height))
    }
}

fn options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

impl InputInjector for PortalInjector {
    fn set_mapping(&self, mapping: Mapping) {
        if let Mapping::Whole(shown) = mapping {
            *self.shown.lock().unwrap_or_else(|e| e.into_inner()) = Some(shown);
        }
    }

    fn move_mouse(&self, pos: MousePosition) -> Result<()> {
        let Some(session) = self.session(DEVICE_POINTER) else { return Ok(()) };
        let Some((node, x, y)) = self.place(&session, pos.x, pos.y) else { return Ok(()) };
        session.notify("NotifyPointerMotionAbsolute", &(session.handle(), options(), node, x, y))
    }

    fn move_mouse_by(&self, dx: i32, dy: i32) -> Result<()> {
        let Some(session) = self.session(DEVICE_POINTER) else { return Ok(()) };
        session.notify("NotifyPointerMotion", &(session.handle(), options(), dx as f64, dy as f64))
    }

    fn mouse_button(&self, input: MouseButtonInput) -> Result<()> {
        let Some(&button) = BUTTONS.get(input.button as usize) else { return Ok(()) };
        let Some(session) = self.session(DEVICE_POINTER) else { return Ok(()) };
        session.notify("NotifyPointerButton", &(session.handle(), options(), button, input.pressed as u32))
    }

    fn scroll(&self, input: ScrollInput) -> Result<()> {
        let Some(session) = self.session(DEVICE_POINTER) else { return Ok(()) };
        let (vertical, horizontal) = {
            let mut remainder = self.scroll_remainder.lock().unwrap_or_else(|e| e.into_inner());
            // The wire's "up" is positive; the desktop's discrete steps count
            // down as positive. Right is positive in both.
            remainder.0 -= input.dy as i32;
            remainder.1 += input.dx as i32;
            let detent = WHEEL_DELTA as i32;
            let steps = (remainder.0 / detent, remainder.1 / detent);
            remainder.0 -= steps.0 * detent;
            remainder.1 -= steps.1 * detent;
            steps
        };
        if vertical != 0 {
            session.notify("NotifyPointerAxisDiscrete", &(session.handle(), options(), AXIS_VERTICAL, vertical))?;
        }
        if horizontal != 0 {
            session.notify("NotifyPointerAxisDiscrete", &(session.handle(), options(), AXIS_HORIZONTAL, horizontal))?;
        }
        Ok(())
    }

    fn key(&self, input: KeyInput) -> Result<()> {
        let code = if input.scancode != 0 {
            evdev_from_scancode(input.scancode, input.extended)
        } else {
            None
        }
        .or_else(|| evdev_from_virtual_key(input.keycode));
        let Some(code) = code else {
            debug!("No Linux key for virtual key {:#x}, scancode {:#x}", input.keycode, input.scancode);
            return Ok(());
        };
        let Some(session) = self.session(DEVICE_KEYBOARD) else { return Ok(()) };
        session.notify("NotifyKeyboardKeycode", &(session.handle(), options(), code, input.pressed as u32))
    }

    fn touch(&self, input: TouchInput) -> Result<()> {
        let Some(session) = self.session(DEVICE_TOUCHSCREEN) else { return Ok(()) };
        let mut touches = self.touches.lock().unwrap_or_else(|e| e.into_inner());
        match input.phase {
            TouchPhase::Start | TouchPhase::Move => {
                let Some((node, x, y)) = self.place(&session, input.x, input.y) else { return Ok(()) };
                let starting = !touches.contains_key(&input.id);
                // A slot per contact, the lowest free one, as a touchscreen
                // numbers its fingers.
                let slot = match touches.get(&input.id) {
                    Some(&slot) => slot,
                    None => {
                        let slot = (0..).find(|s| !touches.values().any(|v| v == s)).unwrap_or(0);
                        touches.insert(input.id, slot);
                        slot
                    }
                };
                let method = if starting { "NotifyTouchDown" } else { "NotifyTouchMotion" };
                session.notify(method, &(session.handle(), options(), node, slot, x, y))
            }
            TouchPhase::End | TouchPhase::Cancel => match touches.remove(&input.id) {
                Some(slot) => session.notify("NotifyTouchUp", &(session.handle(), options(), slot)),
                None => Ok(()),
            },
        }
    }

    fn release_touches(&self) -> Result<()> {
        let slots: Vec<u32> = self.touches.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, s)| s).collect();
        if slots.is_empty() {
            return Ok(());
        }
        let Some(session) = portal::current() else { return Ok(()) };
        for slot in slots {
            session.notify("NotifyTouchUp", &(session.handle(), options(), slot))?;
        }
        Ok(())
    }
}

/// The Linux key code for a PC scancode (set 1), with or without the `E0`
/// prefix.
///
/// Linux numbered its keys after set 1, so the main block maps straight
/// across; the extended keys are listed.
pub fn evdev_from_scancode(scancode: u16, extended: bool) -> Option<i32> {
    let code = scancode as i32;
    if !extended {
        return match code {
            // Windows reports Pause as 0x45 without the prefix and Num Lock as
            // 0x45 with it — the reverse of what set 1 alone would suggest.
            0x45 => Some(119),
            0x01..=0x58 => Some(code),
            0x59 => Some(117),        // keypad =
            0x64..=0x6e => Some(code + 119), // F13..F23
            0x70 => Some(93),         // Katakana/Hiragana
            0x73 => Some(89),         // Ro
            0x76 => Some(194),        // F24
            0x79 => Some(92),         // Henkan
            0x7b => Some(94),         // Muhenkan
            0x7d => Some(124),        // Yen
            _ => None,
        };
    }
    Some(match code {
        0x10 => 165, // previous track
        0x19 => 163, // next track
        0x1c => 96,  // keypad Enter
        0x1d => 97,  // right Ctrl
        0x20 => 113, // mute
        0x22 => 164, // play/pause
        0x24 => 166, // stop
        0x2e => 114, // volume down
        0x30 => 115, // volume up
        0x32 => 172, // browser home
        0x35 => 98,  // keypad /
        0x37 => 99,  // Print Screen
        0x38 => 100, // right Alt
        0x45 => 69,  // Num Lock
        0x47 => 102, // Home
        0x48 => 103, // Up
        0x49 => 104, // Page Up
        0x4b => 105, // Left
        0x4d => 106, // Right
        0x4f => 107, // End
        0x50 => 108, // Down
        0x51 => 109, // Page Down
        0x52 => 110, // Insert
        0x53 => 111, // Delete
        0x5b => 125, // left Windows
        0x5c => 126, // right Windows
        0x5d => 127, // Menu
        0x5e => 116, // Power
        0x5f => 142, // Sleep
        0x63 => 143, // Wake
        _ => return None,
    })
}

/// The Linux key code for a Windows virtual-key code: what a guest without
/// scancodes sends. Mapped by what the key is, on a US layout.
pub fn evdev_from_virtual_key(vk: u16) -> Option<i32> {
    const LETTERS: [i32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, // A..M
        49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, // N..Z
    ];
    const KEYPAD: [i32; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
    Some(match vk {
        0x41..=0x5a => LETTERS[(vk - 0x41) as usize],
        0x30 => 11,
        0x31..=0x39 => (vk - 0x31) as i32 + 2,
        0x60..=0x69 => KEYPAD[(vk - 0x60) as usize],
        0x70..=0x79 => (vk - 0x70) as i32 + 59, // F1..F10
        0x7a => 87,                             // F11
        0x7b => 88,                             // F12
        0x7c..=0x86 => (vk - 0x7c) as i32 + 183, // F13..F23
        0x87 => 194,                            // F24
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0d => 28,  // Enter
        0x10 | 0xa0 => 42, // Shift, left Shift
        0xa1 => 54,  // right Shift
        0x11 | 0xa2 => 29, // Ctrl, left Ctrl
        0xa3 => 97,  // right Ctrl
        0x12 | 0xa4 => 56, // Alt, left Alt
        0xa5 => 100, // right Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x1b => 1,   // Escape
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2c => 99,  // Print Screen
        0x2d => 110, // Insert
        0x2e => 111, // Delete
        0x5b => 125, // left Windows
        0x5c => 126, // right Windows
        0x5d => 127, // Menu
        0x6a => 55,  // keypad *
        0x6b => 78,  // keypad +
        0x6d => 74,  // keypad -
        0x6e => 83,  // keypad .
        0x6f => 98,  // keypad /
        0x90 => 69,  // Num Lock
        0x91 => 70,  // Scroll Lock
        0xad => 113, // mute
        0xae => 114, // volume down
        0xaf => 115, // volume up
        0xb0 => 163, // next track
        0xb1 => 165, // previous track
        0xb2 => 166, // stop
        0xb3 => 164, // play/pause
        0xba => 39,  // ;
        0xbb => 13,  // =
        0xbc => 51,  // ,
        0xbd => 12,  // -
        0xbe => 52,  // .
        0xbf => 53,  // /
        0xc0 => 41,  // `
        0xdb => 26,  // [
        0xdc => 43,  // \
        0xdd => 27,  // ]
        0xde => 40,  // '
        0xe2 => 86,  // the key between left Shift and Z on ISO keyboards
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_block_maps_straight_across() {
        assert_eq!(evdev_from_scancode(0x01, false), Some(1)); // Escape
        assert_eq!(evdev_from_scancode(0x1e, false), Some(30)); // A
        assert_eq!(evdev_from_scancode(0x1c, false), Some(28)); // Enter
        assert_eq!(evdev_from_scancode(0x57, false), Some(87)); // F11
    }

    #[test]
    fn the_extended_keys_are_told_apart_from_their_twins() {
        assert_eq!(evdev_from_scancode(0x1c, true), Some(96), "keypad Enter is not Enter");
        assert_eq!(evdev_from_scancode(0x1d, true), Some(97), "right Ctrl is not left Ctrl");
        assert_eq!(evdev_from_scancode(0x48, true), Some(103), "Up is not keypad 8");
        assert_eq!(evdev_from_scancode(0x48, false), Some(72), "keypad 8");
        assert_eq!(evdev_from_scancode(0x5b, true), Some(125), "the Windows key");
    }

    #[test]
    fn pause_and_num_lock_are_the_right_way_round() {
        // Windows' own convention, which is the reverse of what set 1 alone says.
        assert_eq!(evdev_from_scancode(0x45, false), Some(119), "Pause");
        assert_eq!(evdev_from_scancode(0x45, true), Some(69), "Num Lock");
    }

    #[test]
    fn a_virtual_key_maps_to_the_same_key_as_its_scancode() {
        // A guest with scancodes and one without must press the same key.
        for (vk, scancode, extended) in [
            (0x41u16, 0x1eu16, false), // A
            (0x5a, 0x2c, false),       // Z
            (0x31, 0x02, false),       // 1
            (0x30, 0x0b, false),       // 0
            (0x0d, 0x1c, false),       // Enter
            (0x20, 0x39, false),       // Space
            (0x70, 0x3b, false),       // F1
            (0x7b, 0x58, false),       // F12
            (0x25, 0x4b, true),        // Left
            (0x2e, 0x53, true),        // Delete
            (0xa3, 0x1d, true),        // right Ctrl
            (0x5b, 0x5b, true),        // left Windows
            (0x65, 0x4c, false),       // keypad 5
        ] {
            assert_eq!(
                evdev_from_virtual_key(vk),
                evdev_from_scancode(scancode, extended),
                "virtual key {vk:#x} and scancode {scancode:#x}"
            );
        }
    }

    #[test]
    fn every_letter_has_a_key_and_no_two_share_one() {
        let mut seen = std::collections::BTreeSet::new();
        for vk in 0x41u16..=0x5a {
            let code = evdev_from_virtual_key(vk).expect("a letter");
            assert!(seen.insert(code), "{} shares a key", vk as u8 as char);
        }
    }

    #[test]
    fn without_a_session_input_is_dropped_rather_than_failing() {
        // No capture has opened a portal session in a test.
        let injector = PortalInjector::new();
        injector.move_mouse(MousePosition { x: 100, y: 100 }).unwrap();
        injector.key(KeyInput { keycode: 0x41, pressed: true, scancode: 0, extended: false }).unwrap();
        injector.scroll(ScrollInput { dx: 0, dy: 120 }).unwrap();
        injector.release_touches().unwrap();
    }
}
