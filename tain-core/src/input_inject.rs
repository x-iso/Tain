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

/// One finger, at one moment, in the normalized space the wire uses.
///
/// Mirrors [`crate::protocol::TouchEvent`]; see it for why a single contact
/// travels rather than a whole frame.
#[derive(Debug, Clone, Copy)]
pub struct TouchInput {
    pub id: u8,
    pub phase: crate::protocol::TouchPhase,
    pub x: u16,
    pub y: u16,
    /// `1..=255`, or `0` when the sender did not know.
    pub pressure: u8,
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

    /// Move, place or lift one touch contact.
    ///
    /// The backend owns the set of contacts currently down, because the
    /// platform call needs all of them on every update — see
    /// [`crate::protocol::TouchEvent`].
    fn touch(&self, input: TouchInput) -> Result<()>;

    /// Drop every contact this backend is holding, as cancelled.
    ///
    /// For when the events that would have lifted them are no longer coming
    /// here — the route changed mid-gesture, or the session ended. A no-op when
    /// nothing is down, which is nearly always.
    fn release_touches(&self) -> Result<()>;

    /// Whether this backend actually reaches the operating system.
    fn is_real(&self) -> bool {
        true
    }
}

// ── Windows implementation ────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::sync::Mutex;
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::Input::Pointer::{
        InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
        POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
        POINTER_FLAGS, POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        PT_TOUCH, TOUCH_FLAG_NONE, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_PRESSURE,
    };
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

    pub struct WindowsInjector {
        /// The contacts Windows currently believes are down.
        ///
        /// Behind a mutex because the trait is `Sync` and this is the one piece
        /// of injection that is not stateless. Contended only by touches, which
        /// arrive at digitizer rates from a single guest.
        touch: Mutex<TouchState>,
    }

    /// Everything the touch injector has to remember between events.
    #[derive(Default)]
    struct TouchState {
        /// `None` before the first touch, then whether setup succeeded.
        ///
        /// A machine that refuses touch injection refuses it permanently, so a
        /// failure is remembered rather than retried once per finger.
        ready: Option<bool>,
        /// One entry per live contact, which is what `InjectTouchInput` wants
        /// handed to it whole on every call.
        contacts: Vec<Contact>,
    }

    /// A live contact, in the only fields that describe it.
    ///
    /// Not a `POINTER_TOUCH_INFO`, though that is what gets injected. That
    /// struct carries a window handle and a source-device handle, which makes
    /// it a raw pointer and therefore not `Send` — and this has to live behind
    /// a mutex in a `Sync` injector. Both handles are zero in everything we
    /// send, so keeping four plain fields and building the record at the call
    /// costs nothing and keeps the state ordinary data.
    #[derive(Clone, Copy)]
    struct Contact {
        id: u8,
        point: POINT,
        pressure: u8,
        /// What this contact is doing in the next injection.
        flags: POINTER_FLAGS,
    }

    /// As many fingers as a Windows digitizer reports, which is as many as
    /// anything is designed to interpret. The guest allocates its contact ids
    /// out of the same range.
    const MAX_CONTACTS: u32 = crate::protocol::MAX_TOUCH_CONTACTS as u32;

    /// A guest's normalized position as a pixel inside `shown`.
    ///
    /// Free and taking its rectangle explicitly so the arithmetic can be tested
    /// against display arrangements this machine does not have — which is the
    /// whole class of bug this replaces.
    fn into_pixels(shown: RECT, x: u16, y: u16) -> POINT {
        // A zero span would be a machine with no display. Clamp rather than
        // divide by zero; the injection then fails on its own terms.
        let span = |size: i32| (size.max(1) - 1) as i64;
        let scale = |value: u16, size: i32| {
            ((value as i64 * span(size)) / crate::protocol::ABSOLUTE_MAX as i64) as i32
        };
        POINT {
            x: shown.left + scale(x, shown.right - shown.left),
            y: shown.top + scale(y, shown.bottom - shown.top),
        }
    }

    /// A desktop pixel as a position normalized across the whole desktop.
    fn out_of_desktop(desktop: RECT, point: POINT) -> (i32, i32) {
        // Note the clamp is not the one in `into_pixels`: there the span is a
        // multiplier and zero is a fine answer, here it is the divisor. A
        // machine reporting no display would otherwise divide by zero, which is
        // a panic on the input path rather than an injection that fails.
        let span = |size: i32| ((size - 1).max(1)) as i64;
        // # Rounded up, because Windows rounds down
        //
        // `SendInput` turns this back into a pixel by truncating. Truncating
        // here as well loses a pixel wherever the two divisions fall either side
        // of a boundary — the pointer arrives one pixel up or left of where the
        // guest put it, which is the difference between grabbing a window edge
        // and missing it. Rounding to nearest still loses it half the time.
        //
        // The smallest value that truncates back to the intended pixel is its
        // ceiling, and that value cannot reach the next pixel because a display
        // is never wider than the normalized range. So the round trip is exact
        // by construction rather than close enough.
        let scale = |value: i32, origin: i32, size: i32| {
            let span = span(size);
            let numerator = (value - origin) as i64 * crate::protocol::ABSOLUTE_MAX as i64;
            ((numerator + span - 1) / span) as i32
        };
        (
            scale(point.x, desktop.left, desktop.right - desktop.left),
            scale(point.y, desktop.top, desktop.bottom - desktop.top),
        )
    }

    /// The bounding rectangle of every display, in physical pixels.
    ///
    /// The origin is not `(0, 0)`: a monitor left of or above the primary one
    /// has negative coordinates, and the virtual desktop starts at whichever is
    /// furthest out.
    fn virtual_desktop() -> RECT {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        // SAFETY: documented reads of system metrics, no arguments to get wrong
        // and nothing to release.
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        RECT { left, top, right: left + width, bottom: top + height }
    }

    impl WindowsInjector {
        pub fn new() -> Result<Self> {
            Ok(Self { touch: Mutex::new(TouchState::default()) })
        }

        /// The rectangle of the virtual desktop the guest is looking at.
        ///
        /// The primary display, because that is what capture streams — see
        /// `capture_dxgi::primary_output`, which picks it explicitly so this and
        /// the picture cannot disagree.
        ///
        /// It is a function rather than a field because a display can be added,
        /// removed or rearranged while a session is running, and a pointer that
        /// keeps landing where a monitor used to be is worse than one that costs
        /// two system-metric reads per event.
        ///
        /// When a session can be watching any display this becomes what that
        /// session is watching; the arithmetic below does not change.
        fn shown(&self) -> RECT {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
            };
            // SAFETY: documented reads of system metrics, no arguments to get
            // wrong and nothing to release.
            //
            // The primary display is at the origin of the virtual desktop by
            // definition on Windows, so these two spans are its whole rectangle.
            let (width, height) =
                unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            RECT { left: 0, top: 0, right: width, bottom: height }
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

        /// Normalized guest coordinates as pixels on the virtual desktop.
        ///
        /// # What the guest's numbers actually mean
        ///
        /// They are normalized against **the picture it was sent**, which is one
        /// monitor. This used to spread them across the whole virtual desktop
        /// instead, which is the same space only on a machine with one display.
        /// On any other, a pointer landed proportionally further out the further
        /// right it went, and on the wrong monitor entirely past the halfway
        /// point — while the picture looked completely normal, so the symptom
        /// read as "input is offset" rather than as anything to do with
        /// monitors.
        ///
        /// So the guest's square maps onto [`Self::shown`], and only then into
        /// desktop pixels. The origin matters: a monitor left of the primary one
        /// has negative coordinates, so this is not a scale, it is a scale and
        /// an offset.
        ///
        /// Reading them as *physical* pixels is what per-monitor DPI awareness
        /// buys; a process that ended up merely system aware would be handed a
        /// scaled virtual desktop here and put every touch in the wrong place
        /// on the secondary monitors. See `declare_dpi_awareness` in `tain-cli`.
        fn to_desktop_pixels(&self, x: u16, y: u16) -> POINT {
            into_pixels(self.shown(), x, y)
        }

        /// Desktop pixels back to what `SendInput` wants: normalized across
        /// every monitor, which is what `MOUSEEVENTF_VIRTUALDESK` means.
        fn to_virtual_desktop(point: POINT) -> (i32, i32) {
            out_of_desktop(virtual_desktop(), point)
        }

        /// Build the record Windows wants from the fields we keep.
        fn to_pointer_info(contact: &Contact) -> POINTER_TOUCH_INFO {
            // A digitizer reports an *area*, and `TOUCH_MASK_CONTACTAREA`
            // promises one. Four pixels square is a fingertip as far as
            // anything reading this is concerned, and small enough that the
            // point stays the point.
            const RADIUS: i32 = 2;

            let point = contact.point;
            POINTER_TOUCH_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_TOUCH,
                    pointerId: contact.id as u32,
                    pointerFlags: contact.flags,
                    // `hwndTarget` is left zero: that means "whatever is under
                    // the point", which is what a real finger does.
                    ptPixelLocation: point,
                    ..Default::default()
                },
                touchFlags: TOUCH_FLAG_NONE,
                touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE,
                rcContact: RECT {
                    left: point.x - RADIUS,
                    top: point.y - RADIUS,
                    right: point.x + RADIUS,
                    bottom: point.y + RADIUS,
                },
                pressure: to_windows_pressure(contact.pressure),
                ..Default::default()
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
            // # Two conversions, not none
            //
            // Both spaces are `0..65535`, which is why this used to hand the
            // guest's numbers straight to `SendInput` and looked right. They are
            // not the same space: the guest's are across the picture it was
            // sent, and `VIRTUALDESK` reads them as across every monitor. On one
            // display those coincide. On two they do not, and the pointer lands
            // on the wrong one.
            //
            // So: into desktop pixels through the rectangle actually being
            // shown, then back out across the whole desktop, which is what the
            // flag promises.
            let point = self.to_desktop_pixels(pos.x, pos.y);
            let (x, y) = Self::to_virtual_desktop(point);
            Self::send(Self::mouse(
                MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
                x,
                y,
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

        /// Place, move or lift one contact, and re-assert every other one.
        ///
        /// # Why the whole set goes every time
        ///
        /// `InjectTouchInput` does not take a change, it takes a state: the
        /// complete list of contacts that are down at this instant. A contact
        /// left out of a call is a contact that has been lifted, whatever the
        /// sender meant. So the fingers resting still are re-sent, flagged
        /// `UPDATE`, alongside the one that actually did something.
        ///
        /// # Why an unknown contact is not an error
        ///
        /// The two ends can disagree about what is down — a guest that
        /// reconnects mid-gesture, a `Start` that did not survive, a physical
        /// touch on the host that resets the injection out from under us. Every
        /// disagreement here is resolved towards *fewer stuck fingers*: a move
        /// for a contact nobody knows about becomes a press, and a lift for one
        /// becomes nothing at all. Refusing them instead would leave a finger
        /// pinned to the host's screen with no message able to free it.
        fn touch(&self, input: TouchInput) -> Result<()> {
            use crate::protocol::TouchPhase;

            let mut state = self.touch.lock().unwrap_or_else(|e| e.into_inner());

            if state.ready.is_none() {
                // SAFETY: a documented call taking two constants.
                let started =
                    unsafe { InitializeTouchInjection(MAX_CONTACTS, TOUCH_FEEDBACK_DEFAULT) };
                match &started {
                    Ok(()) => info!("Touch injection: ready for up to {MAX_CONTACTS} contacts"),
                    Err(e) => warn!("Touch injection is not available on this machine: {e}"),
                }
                state.ready = Some(started.is_ok());
            }
            if state.ready != Some(true) {
                // Already reported once. Not an error per finger.
                return Ok(());
            }

            /// A contact that stays down between updates.
            const HELD: POINTER_FLAGS = POINTER_FLAGS(
                POINTER_FLAG_UPDATE.0 | POINTER_FLAG_INRANGE.0 | POINTER_FLAG_INCONTACT.0,
            );

            let point = self.to_desktop_pixels(input.x, input.y);
            let existing = state.contacts.iter().position(|c| c.id == input.id);

            match (input.phase, existing) {
                // Gone already, or never here. Nothing to lift, and nothing to
                // tell Windows: injecting the remaining contacts unchanged
                // would be a frame that says nothing happened.
                (TouchPhase::End | TouchPhase::Cancel, None) => return Ok(()),

                (TouchPhase::End, Some(index)) => {
                    state.contacts[index].flags = POINTER_FLAG_UP;
                }
                (TouchPhase::Cancel, Some(index)) => {
                    state.contacts[index].flags = POINTER_FLAG_UP | POINTER_FLAG_CANCELED;
                }

                // Down, and known to be down. Includes a `Start` for an id
                // already in use, which is the guest and this disagreeing: the
                // finger is here now, so treat it as being here now.
                (TouchPhase::Start | TouchPhase::Move, Some(index)) => {
                    state.contacts[index] =
                        Contact { id: input.id, point, pressure: input.pressure, flags: HELD };
                }

                // New. Also where a `Move` for an unknown contact lands.
                (TouchPhase::Start | TouchPhase::Move, None) => {
                    if state.contacts.len() >= MAX_CONTACTS as usize {
                        debug!("Ignoring touch {}: already at {MAX_CONTACTS} contacts", input.id);
                        return Ok(());
                    }
                    state.contacts.push(Contact {
                        id: input.id,
                        point,
                        pressure: input.pressure,
                        flags: POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                    });
                }
            }

            let frame: Vec<POINTER_TOUCH_INFO> =
                state.contacts.iter().map(Self::to_pointer_info).collect();
            // SAFETY: a slice of fully initialised records, never empty — every
            // branch above either returned or left at least one contact.
            let injected = unsafe { InjectTouchInput(&frame) };

            // Whatever happened, a contact that was lifted is no longer ours.
            state.contacts.retain(|c| c.flags & POINTER_FLAG_UP != POINTER_FLAG_UP);
            // And one that just went down is held from here on: `DOWN` twice
            // for the same id is rejected.
            for contact in &mut state.contacts {
                contact.flags = HELD;
            }

            if let Err(e) = injected {
                // Most often the injection was reset by a real touch on the
                // host, which invalidates every contact at once. Dropping the
                // set lets the next event start a fresh gesture instead of
                // failing against a state Windows has already forgotten.
                state.contacts.clear();
                anyhow::bail!("InjectTouchInput failed: {e}");
            }
            Ok(())
        }

        /// Cancel everything still down, in one frame.
        ///
        /// Cancelled rather than lifted: an application reading these should
        /// undo whatever the gesture was doing, not complete it. Nothing here
        /// knows whether the finger is still on the glass — only that its
        /// events stopped arriving.
        fn release_touches(&self) -> Result<()> {
            let mut state = self.touch.lock().unwrap_or_else(|e| e.into_inner());
            if state.contacts.is_empty() {
                return Ok(());
            }
            for contact in &mut state.contacts {
                contact.flags = POINTER_FLAG_UP | POINTER_FLAG_CANCELED;
            }
            let frame: Vec<POINTER_TOUCH_INFO> =
                state.contacts.iter().map(Self::to_pointer_info).collect();
            // SAFETY: a non-empty slice of fully initialised records.
            let injected = unsafe { InjectTouchInput(&frame) };
            // Ours either way: a failure here means Windows has already
            // forgotten them, which is the state we were asking for.
            state.contacts.clear();
            debug!("Cancelled {} touch contact(s) that stopped being updated", frame.len());
            injected.map_err(|e| anyhow::anyhow!("could not cancel touch contacts: {e}"))
        }
    }

    /// Map `1..=255` onto the `1..=1024` Windows uses.
    ///
    /// `0` — the sender had no pressure to report — becomes 512, which is what
    /// Windows documents as the value for a contact that does not say.
    fn to_windows_pressure(pressure: u8) -> u32 {
        if pressure == 0 {
            return 512;
        }
        ((pressure as u32 * 1024) / 255).clamp(1, 1024)
    }

    #[cfg(test)]
    mod pointer_space_tests {
        use super::*;

        const MAX: u16 = crate::protocol::ABSOLUTE_MAX;

        fn rect(left: i32, top: i32, width: i32, height: i32) -> RECT {
            RECT { left, top, right: left + width, bottom: top + height }
        }

        /// Two monitors side by side: the picture is the left one.
        ///
        /// # The bug this pins
        ///
        /// Guest coordinates were handed to `SendInput` unchanged, because both
        /// spaces are `0..65535` and on a single-monitor machine they are the
        /// same space. With a second monitor they are not: the far right of the
        /// picture is the middle of the desktop, so every position landed at
        /// roughly twice its distance from the left edge and anything past the
        /// halfway point landed on a monitor the guest could not even see.
        #[test]
        fn a_second_monitor_does_not_stretch_the_pointer_onto_it() {
            let shown = rect(0, 0, 1920, 1080);
            let desktop = rect(0, 0, 3840, 1080);

            // The far right of the picture is the right edge of the *first*
            // monitor, which is the middle of the desktop.
            let right = out_of_desktop(desktop, into_pixels(shown, MAX, 0)).0;
            let middle = i32::from(MAX) / 2;
            assert!(
                (right - middle).abs() <= 16,
                "the right edge of the picture mapped to {right}, not about {middle} — \
                 which is a pointer on the wrong monitor"
            );

            // And the two ends still reach the two ends of what is shown.
            assert_eq!(into_pixels(shown, 0, 0).x, 0);
            assert_eq!(into_pixels(shown, MAX, 0).x, 1919);
        }

        /// A monitor to the left of the primary one has negative coordinates.
        ///
        /// The reason this is an offset and not only a scale. The primary
        /// display is always at the desktop origin, so a monitor placed left of
        /// it moves the *desktop* origin negative and everything shown sits in
        /// the right-hand half of it.
        #[test]
        fn a_monitor_left_of_the_primary_moves_the_origin_not_the_picture() {
            let shown = rect(0, 0, 1920, 1080);
            let desktop = rect(-1920, 0, 3840, 1080);

            // The left edge of the picture is the middle of the desktop.
            let left = out_of_desktop(desktop, into_pixels(shown, 0, 0)).0;
            let middle = i32::from(MAX) / 2;
            assert!(
                (left - middle).abs() <= 16,
                "the left edge of the picture mapped to {left}, not about {middle}"
            );
            // And the far right is the far right of the desktop.
            assert_eq!(out_of_desktop(desktop, into_pixels(shown, MAX, 0)).0, i32::from(MAX));
        }

        /// Every pixel survives the trip out to normalized space and back.
        ///
        /// # Why every one, and not a few
        ///
        /// This is the chain a real pointer takes: the guest's position becomes
        /// a host pixel, that becomes a normalized value for `SendInput`, and
        /// Windows turns it back into a pixel by truncating. A pixel lost
        /// anywhere in there puts the pointer one place off — enough to miss a
        /// window edge or a one-pixel border, and invisible in any test that
        /// samples a handful of values, because the loss happens only where two
        /// divisions land either side of a boundary.
        ///
        /// It holds exactly rather than approximately — see `out_of_desktop` —
        /// so exhausting the display is both affordable and the honest test of
        /// the claim.
        #[test]
        fn every_pixel_survives_the_round_trip() {
            for &(width, height) in
                &[(2560, 1600), (1920, 1080), (3840, 2160), (1366, 768), (1, 1)]
            {
                let display = rect(0, 0, width, height);
                for pixel in 0..width {
                    let (x, _) = out_of_desktop(display, POINT { x: pixel, y: 0 });
                    assert!(
                        (0..=i32::from(MAX)).contains(&x),
                        "{width}x{height}: pixel {pixel} left the normalized range at {x}"
                    );
                    let back = into_pixels(display, x as u16, 0).x;
                    assert_eq!(back, pixel, "{width}x{height}: pixel {pixel} came back as {back}");
                }
            }
        }

        /// The same, on a display that is not at the desktop origin.
        #[test]
        fn a_display_away_from_the_origin_survives_it_too() {
            // A 1920x1080 monitor to the right of a 2560-wide primary.
            let desktop = rect(0, 0, 4480, 1600);
            let shown = rect(2560, 0, 1920, 1080);
            for pixel in shown.left..shown.right {
                let (x, _) = out_of_desktop(desktop, POINT { x: pixel, y: 0 });
                let back = into_pixels(desktop, x as u16, 0).x;
                assert_eq!(back, pixel, "pixel {pixel} on the second monitor came back as {back}");
            }
        }

        /// A machine reporting no display must not divide by zero.
        #[test]
        fn a_desktop_with_no_extent_is_survivable() {
            let nothing = rect(0, 0, 0, 0);
            let _ = out_of_desktop(nothing, into_pixels(nothing, MAX, MAX));
        }
    }

    #[cfg(test)]
    mod touch_tests {
        use super::*;

        #[test]
        fn unreported_pressure_becomes_the_windows_default() {
            assert_eq!(to_windows_pressure(0), 512);
        }

        #[test]
        fn pressure_spans_the_windows_range_without_leaving_it() {
            assert_eq!(to_windows_pressure(255), 1024);
            assert!((1..=1024).contains(&to_windows_pressure(1)));
            assert!((1..=1024).contains(&to_windows_pressure(128)));
        }

        /// Normalized extremes land on the edges of the *shown display*.
        ///
        /// This used to assert the edges of the whole virtual desktop, which is
        /// the bug rather than the behaviour: it is only the same answer on a
        /// machine with one display, which is what every machine that ran it
        /// had. See `pointer_space_tests`, which covers the arrangements this
        /// one cannot.
        #[test]
        fn normalized_extremes_land_on_the_edges_of_the_shown_display() {
            let Ok(injector) = WindowsInjector::new() else { return };
            let shown = injector.shown();
            if shown.right - shown.left <= 0 {
                return; // No display; nothing to be right about.
            }

            let start = injector.to_desktop_pixels(0, 0);
            let end = injector.to_desktop_pixels(u16::MAX, u16::MAX);
            assert_eq!(start.x, shown.left, "zero should be the shown display's left edge");
            assert_eq!(
                end.x,
                shown.right - 1,
                "the maximum should be the shown display's right edge, not the desktop's"
            );

            // On a machine with a second monitor, the two answers differ — and
            // that difference is the whole point. Silent where there is only one.
            let desktop = virtual_desktop();
            if desktop.right > shown.right {
                assert_ne!(
                    end.x,
                    desktop.right - 1,
                    "the pointer reached the far edge of another monitor"
                );
            }
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

    fn touch(&self, input: TouchInput) -> Result<()> {
        trace!("StubInjector: touch {} {:?}", input.id, input.phase);
        Ok(())
    }

    fn release_touches(&self) -> Result<()> {
        trace!("StubInjector: releasing touch contacts");
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
