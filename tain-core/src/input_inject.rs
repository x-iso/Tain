//! Input event injection into the host operating system.
//!
//! Coordinates arriving from the guest are already normalized to `0..=65535`
//! across the full screen (see [`crate::protocol::MouseEvent`]), and keycodes are
//! always Windows virtual-key codes. Each backend translates from those two
//! canonical forms into its platform's API.

use anyhow::Result;
use tracing::*;

/// Absolute pointer position, normalized to `0..=65535` across the whole screen.
#[derive(Debug, Clone, Copy)]
pub struct MousePosition {
    pub x: u16,
    pub y: u16,
}

/// A single keyboard input event.
#[derive(Debug, Clone, Copy)]
pub struct KeyInput {
    /// Windows virtual-key code.
    pub keycode: u16,
    /// Whether the key is pressed (true) or released (false).
    pub pressed: bool,
    /// Hardware scancode, or `0` when the sender had none.
    ///
    /// Preferred over `keycode` when present — see
    /// [`crate::protocol::KeyEvent::scancode`] for why.
    pub scancode: u16,
    /// Whether the scancode carries the extended (`E0`) prefix.
    pub extended: bool,
}

/// A single mouse button event.
#[derive(Debug, Clone, Copy)]
pub struct MouseButtonInput {
    /// Button index: 0=left, 1=right, 2=middle.
    pub button: u8,
    /// Whether pressed (true) or released (false).
    pub pressed: bool,
}

/// Wheel movement in wheel-delta units (120 per detent).
#[derive(Debug, Clone, Copy)]
pub struct ScrollInput {
    pub dx: i16,
    pub dy: i16,
}

/// Trait for injecting input events into the host OS.
pub trait InputInjector: Send + Sync {
    /// Move the pointer to an absolute position on the host display.
    fn move_mouse(&self, pos: MousePosition) -> Result<()>;

    /// Press or release a mouse button.
    fn mouse_button(&self, input: MouseButtonInput) -> Result<()>;

    /// Scroll the mouse wheel.
    fn scroll(&self, input: ScrollInput) -> Result<()>;

    /// Press or release a keyboard key.
    fn key(&self, input: KeyInput) -> Result<()>;

    /// Whether this backend actually reaches the operating system.
    fn is_real(&self) -> bool {
        true
    }
}

// ── Windows implementation ────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
    };

    /// Keys that must carry `KEYEVENTF_EXTENDEDKEY` or Windows treats them as
    /// their numpad twins (arrows become numpad digits, Insert becomes Num0…).
    const EXTENDED_KEYS: &[u16] = &[
        0x21, // PRIOR (PageUp)
        0x22, // NEXT (PageDown)
        0x23, // END
        0x24, // HOME
        0x25, // LEFT
        0x26, // UP
        0x27, // RIGHT
        0x28, // DOWN
        0x2D, // INSERT
        0x2E, // DELETE
        0x5B, // LWIN
        0x5C, // RWIN
        0x6F, // DIVIDE (numpad /)
        0xA3, // RCONTROL
        0xA5, // RMENU (right Alt)
    ];

    pub struct WindowsInjector;

    impl WindowsInjector {
        pub fn new() -> Result<Self> {
            Ok(Self)
        }

        /// Submit one input record, mapping the Win32 failure convention to a Result.
        fn send(input: INPUT) -> Result<()> {
            // SAFETY: `input` is a fully initialised INPUT of the size we declare.
            let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
            if sent == 1 {
                Ok(())
            } else {
                // 0 means the input was blocked — commonly UIPI, when the target
                // window runs elevated and we do not.
                anyhow::bail!(
                    "SendInput was blocked ({}). A window running as administrator \
                     will not accept input from a non-elevated process.",
                    std::io::Error::last_os_error()
                )
            }
        }

        fn mouse(flags: u32, dx: i32, dy: i32, data: i32) -> INPUT {
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: data as u32,
                        dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(
                            flags,
                        ),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
    }

    impl InputInjector for WindowsInjector {
        fn move_mouse(&self, pos: MousePosition) -> Result<()> {
            // Guest coordinates are already in the 0..65535 space SendInput wants,
            // so there is nothing to scale. VIRTUALDESK spans all monitors.
            Self::send(Self::mouse(
                MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
                pos.x as i32,
                pos.y as i32,
                0,
            ))
        }

        fn mouse_button(&self, input: MouseButtonInput) -> Result<()> {
            let flag = match (input.button, input.pressed) {
                (0, true) => MOUSEEVENTF_LEFTDOWN,
                (0, false) => MOUSEEVENTF_LEFTUP,
                (1, true) => MOUSEEVENTF_RIGHTDOWN,
                (1, false) => MOUSEEVENTF_RIGHTUP,
                (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                (2, false) => MOUSEEVENTF_MIDDLEUP,
                (other, _) => {
                    debug!("Ignoring unsupported mouse button {other}");
                    return Ok(());
                }
            };
            Self::send(Self::mouse(flag.0, 0, 0, 0))
        }

        fn scroll(&self, input: ScrollInput) -> Result<()> {
            if input.dy != 0 {
                Self::send(Self::mouse(MOUSEEVENTF_WHEEL.0, 0, 0, input.dy as i32))?;
            }
            if input.dx != 0 {
                Self::send(Self::mouse(MOUSEEVENTF_HWHEEL.0, 0, 0, input.dx as i32))?;
            }
            Ok(())
        }

        /// Inject one key, by scancode where the sender had one.
        ///
        /// # Why the scancode is preferred
        ///
        /// A virtual key is Windows' *interpretation* of a keystroke under the
        /// active keyboard layout. Sending one across asks the far machine to
        /// re-derive the same key using **its** layout, and the two need not
        /// agree. Ordinary letters survive that round trip, which is why this
        /// looked fine for a long time; modifiers, system keys and anything
        /// layout-sensitive do not, which is why Alt, Win and Tab were the
        /// keys that failed.
        ///
        /// A scancode is the position on the keyboard, before interpretation.
        /// With `KEYEVENTF_SCANCODE` it reproduces the same physical key
        /// regardless of either side's layout. This is what RustDesk's "Map"
        /// mode sends, and it is the approach that works.
        ///
        /// The virtual key remains the fallback for senders that have no
        /// scancode to give: the egui path, and modifier releases synthesised
        /// rather than observed.
        fn key(&self, input: KeyInput) -> Result<()> {
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if !input.pressed {
                flags |= KEYEVENTF_KEYUP;
            }

            let by_scancode = input.scancode != 0;
            if by_scancode {
                flags |= KEYEVENTF_SCANCODE;
                // The extended prefix is part of the scancode's identity:
                // without it right Alt becomes left Alt, and the arrow keys
                // become the numpad.
                if input.extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
            } else if EXTENDED_KEYS.contains(&input.keycode) {
                // Injecting by virtual key means Windows derives the scancode,
                // and it needs telling which of the two candidates to pick.
                flags |= KEYEVENTF_EXTENDEDKEY;
            }

            Self::send(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        // With KEYEVENTF_SCANCODE the virtual key is ignored and
                        // must be zero; without it the scancode is.
                        wVk: if by_scancode { VIRTUAL_KEY(0) } else { VIRTUAL_KEY(input.keycode) },
                        wScan: input.scancode,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            })
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::WindowsInjector;

// ── Stub injector for platforms without a backend yet ────────

/// Logs events without touching the OS. Used on macOS and Linux until those
/// backends exist, so the rest of the pipeline stays testable.
pub struct StubInjector;

impl InputInjector for StubInjector {
    fn move_mouse(&self, pos: MousePosition) -> Result<()> {
        trace!("StubInjector: move to ({}, {})", pos.x, pos.y);
        Ok(())
    }

    fn mouse_button(&self, input: MouseButtonInput) -> Result<()> {
        trace!("StubInjector: button {} pressed={}", input.button, input.pressed);
        Ok(())
    }

    fn scroll(&self, input: ScrollInput) -> Result<()> {
        trace!("StubInjector: scroll dx={} dy={}", input.dx, input.dy);
        Ok(())
    }

    fn key(&self, input: KeyInput) -> Result<()> {
        trace!("StubInjector: key {} pressed={}", input.keycode, input.pressed);
        Ok(())
    }

    fn is_real(&self) -> bool {
        false
    }
}

/// Create the appropriate input injector for the current platform.
pub fn create_injector() -> Box<dyn InputInjector> {
    #[cfg(target_os = "windows")]
    {
        match WindowsInjector::new() {
            Ok(injector) => {
                info!("Input injection: Windows SendInput");
                return Box::new(injector);
            }
            Err(e) => warn!("Failed to create Windows injector: {e}"),
        }
    }

    warn!(
        "Input injection is not implemented on this platform — events will be \
         logged but not delivered."
    );
    Box::new(StubInjector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_itself_as_not_real() {
        assert!(!StubInjector.is_real());
        assert!(StubInjector.move_mouse(MousePosition { x: 0, y: 0 }).is_ok());
        assert!(StubInjector
            .key(KeyInput { keycode: 65, pressed: true, scancode: 0, extended: false })
            .is_ok());
    }

    #[test]
    fn unsupported_mouse_buttons_are_ignored_not_errors() {
        // A guest sending a button we don't map must not tear down the session.
        let injector = create_injector();
        assert!(injector
            .mouse_button(MouseButtonInput { button: 9, pressed: true })
            .is_ok());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_backend_is_selected_and_reports_as_real() {
        assert!(create_injector().is_real(), "Windows should get a real injector");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn zero_scroll_deltas_send_nothing() {
        let injector = WindowsInjector::new().unwrap();
        // Both axes zero: no SendInput call at all, so this cannot fail.
        assert!(injector.scroll(ScrollInput { dx: 0, dy: 0 }).is_ok());
    }
}
