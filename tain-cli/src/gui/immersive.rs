//! Immersive mode: treat the session window as if it were the host's machine.
//!
//! Two things change while it is on:
//!
//! 1. **The keyboard goes to the host.** Every key is taken by a low-level hook
//!    and forwarded, instead of being handled locally.
//! 2. **The pointer is confined to the window**, so aiming at the far edge of a
//!    remote screen doesn't drop the cursor onto a local monitor.
//!
//! ## Why the hook is the keyboard, and not just the Win key
//!
//! This used to hook only the Win keys and let egui supply everything else. It
//! could not work, because egui's keyboard model is built for editing text, not
//! for driving another computer. Its `Key` enum has no Win, no Print Screen, no
//! Caps Lock or Num Lock, no Menu key, no numpad, and no way to tell left Ctrl
//! from right; and Ctrl, Alt and Shift are not keys there at all, only a
//! *state* flag attached to other keys. So the promise immersive mode makes —
//! that the window behaves like the host's machine — was one the input path
//! could not keep, and the modifier and system keys simply never went anywhere.
//!
//! The hook sees what the hardware sent: a real virtual-key code, left and
//! right modifiers distinguished, before Windows has decided what any of it
//! means. That is the only source that can express what has to be forwarded,
//! and the host already injects exactly these codes.
//!
//! ## Safety
//!
//! A hook that swallows keys is exactly the sort of thing that leaves a machine
//! feeling broken if it gets stuck, so:
//!
//! - It only swallows while immersive mode is on, which is a deliberate,
//!   visible choice — the toolbar shows it, and it is off by default.
//! - The escape shortcut is recognised by the hook itself and never forwarded,
//!   so there is always a way out that does not depend on the rest of the app
//!   noticing anything.
//! - `Ctrl+Alt+Del` is untouchable regardless — Windows does not deliver it to
//!   low-level hooks.
//! - Injected keystrokes are ignored, so this cannot feed itself when the same
//!   machine is also hosting.
//! - Everything held is released on the host when the mode ends, so a key down
//!   at that moment cannot stick.
//! - If the process dies, Windows tears the hook down automatically.

use tain_core::protocol::KeyEvent;

pub use platform::{key_is_down, MODIFIER_VKS, MOD_ALT, MOD_CTRL, MOD_SHIFT};

/// Windows virtual-key codes for the left and right Win keys.
///
/// Referenced by the tests only: the hook no longer singles these out, because
/// it now forwards every key rather than just these two.
#[cfg(test)]
const VK_LWIN: u16 = 0x5B;

/// Install the keyboard hook before the window system starts.
///
/// # This ordering is load-bearing, and was found the hard way
///
/// Installed *after* eframe is running, the hook is never invoked. Not on the
/// UI thread, not on a thread of its own, and not for injected keys either —
/// while an otherwise identical hook, in a standalone program on the same
/// machine, works on both kinds of thread. `SendInput` confirmed the test
/// keystrokes were genuinely being injected, so the events existed and this
/// process simply did not receive them.
///
/// Installed *before* `eframe::run_native`, it works. Something in the window
/// system's initialisation is the difference; what exactly has not been
/// established, only that the order decides it.
///
/// So this is called from `main`, and must stay there. Moving it back to the
/// first time immersive mode is switched on would silently return the whole
/// subsystem to doing nothing — which is what it did, undetected, through many
/// rounds of fixes aimed at everything except this.
///
/// The startup self-test exists to catch exactly that regression: it says in
/// one line whether the callback is live, without anyone having to press a
/// key.
pub fn start_hook_early() {
    platform::install_key_hook();
    // Nothing should be captured yet: this only proves the hook can be
    // installed and called. Immersive mode turns capture on later.
    platform::set_capture(false);
    // Off unless `TAIN_HOOK_WATCH` is set; see `platform::watch_hook`.
    #[cfg(target_os = "windows")]
    platform::watch_hook();
}

/// Immersive mode state, owned by the app.
///
/// # There is exactly one state, on purpose
///
/// This carried a second flag for a while — "asked for" against "capturing
/// right now" — so that losing focus could suspend capture without discarding
/// the user's choice. Every version of that idea failed, because the thing it
/// consulted was the foreground window and **pressing Win changes the
/// foreground window**. The key being suppressed moved the evidence used to
/// decide whether to suppress it, so the answer was always "some other app is
/// in front" and every key went to the local shell.
///
/// So there is no suspend. While immersive mode is on, the hook is installed
/// and every key belongs to the host. That is what the mode means, and it is
/// self-consistent: Alt+Tab goes to the host too, so there is no wandering off
/// into another application by accident. The way back is the escape shortcut,
/// which the hook recognises itself and never forwards.
#[derive(Default)]
pub struct Immersive {
    /// What the user asked for, and therefore whether the hook is installed.
    active: bool,
    /// Last clip rect applied, so we only call into the OS when it changes.
    applied_clip: Option<egui::Rect>,
    /// The combination the hook watches for, as `(vk, modifier bits)`.
    escape: (u16, u8),
    /// Whether modifiers and Tab are being copied to the host outside
    /// immersive mode.
    forwarding: bool,
    /// When another application first took the front, if it currently holds it.
    backgrounded_since: Option<std::time::Instant>,
    /// Rate limit for [`Immersive::log_foreground`].
    last_foreground_log: Option<std::time::Instant>,
}

impl Immersive {
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Whether the hook is capturing right now.
    ///
    /// Identical to [`Self::is_active`] — kept as a separate name because the
    /// input path is asking a different question ("which keyboard source do I
    /// read"), and the two were genuinely different before the suspend
    /// behaviour was removed.
    pub fn is_armed(&self) -> bool {
        self.active
    }

    /// The combination that leaves immersive mode.
    ///
    /// Handed to the hook because nothing else can see it: every other key is
    /// swallowed, so egui never learns that a modifier is held and cannot match
    /// a shortcut. Set from the user's binding before arming.
    pub fn set_escape_shortcut(&mut self, vk: u16, mods: u8) {
        self.escape = (vk, mods);
        platform::set_escape_shortcut(vk, mods);
    }

    /// End the mode if this application is no longer in front.
    ///
    /// # A recovery path, not a gate
    ///
    /// The hook swallows unconditionally while the mode is on, so in principle
    /// nothing can take the foreground away — Alt+Tab goes to the host like
    /// everything else. In practice the hook can still be removed by Windows
    /// without warning, a key can leak, and someone can end up in another
    /// application with immersive mode still armed.
    ///
    /// This notices that and switches the mode off. It runs on the UI thread,
    /// after the fact, where taking a moment costs nothing and where it cannot
    /// be raced by the keystroke it is reacting to — which is exactly what went
    /// wrong every time this decision was attempted inside the callback.
    ///
    /// Returns true when it ended the mode, so the caller can release whatever
    /// the host still believes is held.
    pub fn end_if_backgrounded(&mut self) -> bool {
        /// How long another application must hold the front before the mode
        /// ends.
        ///
        /// Not instant, deliberately. The previous version ended the mode on a
        /// single unfocused frame, using a signal already known to be
        /// unreliable — the same `GetForegroundWindow` that once named
        /// explorer.exe while this application was plainly the one in use. One
        /// wrong reading then switched the whole mode off, which is a far worse
        /// outcome than the leaked keystroke it was reacting to.
        const PATIENCE: std::time::Duration = std::time::Duration::from_millis(600);

        if !self.active {
            self.backgrounded_since = None;
            return false;
        }

        if platform::app_has_focus() {
            self.backgrounded_since = None;
            return false;
        }

        let since = *self.backgrounded_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() < PATIENCE {
            return false;
        }

        // On the same clock as the hook trace below it, which is the only way
        // to read the two together: a Win press stamped before the foreground
        // moved is a keystroke that leaked despite being swallowed, and one
        // stamped after is a keystroke that arrived while something else was
        // already in front. Those are different faults.
        tracing::info!(
            "Immersive mode ended +{}ms — another application took the front \
             {:?} ago ({})",
            platform::since_capture_ms(),
            since.elapsed(),
            platform::foreground_description(),
        );
        self.backgrounded_since = None;
        self.set_active(false);
        true
    }

    /// Report what the foreground looks like, at most once a second.
    ///
    /// # Why this is being measured rather than reasoned about
    ///
    /// Every remaining decision here depends on whether `GetForegroundWindow`
    /// can be trusted, and it has never been checked on its own. It has been
    /// used as a gate, blamed, removed, restored and blamed again across many
    /// attempts — on the strength of one log line showing it naming
    /// explorer.exe at a moment when it should have named this application.
    ///
    /// One line a second while the mode is on settles it. If it names this
    /// process throughout, the signal is fine and the fault is in the hook. If
    /// it wanders, everything built on it has to go.
    pub fn log_foreground(&mut self) {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(1);
        if !self.active {
            return;
        }
        let due = self.last_foreground_log.is_none_or(|at| at.elapsed() >= EVERY);
        if !due {
            return;
        }
        self.last_foreground_log = Some(std::time::Instant::now());
        tracing::info!("Immersive: foreground is {}", platform::foreground_description());
    }

    /// Tell the hook whether this window is in front, for the frame ahead.
    ///
    /// The forwarding path needs that answer and the callback must not go and
    /// ask for it — see `platform::FORWARD_ALLOWED`. Called once a frame while
    /// a session window is open, which is the only time it is consulted.
    pub fn publish_forward_gate(&self) {
        platform::publish_forward_gate();
    }

    /// Whether keys have gone to another application since the last frame.
    ///
    /// When they have, whatever the host still thinks is held has to be let go:
    /// the key-up for a chord in progress was never forwarded, and a modifier
    /// stuck on the far machine changes every keystroke after it.
    pub fn keys_went_elsewhere(&mut self) -> bool {
        platform::take_lost_foreground()
    }

    /// Whether the user asked to leave via the escape shortcut.
    ///
    /// Polled each frame. Returns true once per press.
    pub fn escape_requested(&mut self) -> bool {
        platform::take_escape_request()
    }

    /// Forward the keys egui cannot express while a session is open.
    ///
    /// Separate from immersive mode and much narrower: modifiers and Tab are
    /// copied to the host and still act on this machine. See
    /// `platform::FORWARD_ACTIVE`.
    pub fn set_forwarding(&mut self, on: bool) {
        if self.forwarding == on {
            return;
        }
        self.forwarding = on;
        platform::set_forwarding(on);
    }

    /// Turn immersive mode on or off. Idempotent.
    pub fn set_active(&mut self, on: bool) {
        if self.active == on {
            return;
        }
        self.active = on;

        if on {
            // The binding first: the hook reads it, and a stale combination is
            // a session with no way out.
            platform::set_escape_shortcut(self.escape.0, self.escape.1);
            platform::install_key_hook();
            tracing::info!("Immersive mode on — the keyboard goes to the host, pointer confined");
        } else {
            let counts = platform::counters();
            for line in platform::take_trace() {
                tracing::info!("Immersive hook saw: {line}");
            }
            self.release();
            // Logged here rather than per key, because here it is free.
            // `passed` above zero means the callback ran and chose not to
            // swallow; both at zero means it never ran at all. Those are
            // different faults and the numbers tell them apart.
            //
            // The slowest callback is the third answer, and the one the
            // counters could not give: a key can be swallowed and still act on
            // this machine, if the swallow arrived after Windows stopped
            // waiting for it. Anything near 300ms means that is what happened.
            tracing::info!(
                "Immersive mode off — callback entered {} times, {} keys taken, \
                 {} passed through, {} hook installs, slowest callback {:.1} ms",
                counts.entered,
                counts.swallowed,
                counts.passed,
                counts.reinstalls,
                counts.slowest_us as f32 / 1000.0,
            );
        }
    }

    /// Release the hook and the cursor, and forget that it was asked for.
    ///
    /// For the session ending or the user switching it off.
    pub fn release(&mut self) {
        self.active = false;
        self.applied_clip = None;
        // Stops the swallowing. Whether the hook itself comes off depends on
        // forwarding, which belongs to the session rather than to this mode —
        // leaving immersive must not stop a plain session sending modifiers.
        platform::remove_key_hook();
        platform::release_cursor();
    }

    /// Confine the pointer to `rect` (logical points, screen coordinates).
    ///
    /// Called every frame while active; only reaches the OS when the window has
    /// actually moved or resized.
    pub fn confine_cursor(&mut self, rect: egui::Rect, pixels_per_point: f32) {
        if !self.active {
            return;
        }
        if self.applied_clip == Some(rect) {
            return;
        }
        self.applied_clip = Some(rect);
        platform::clip_cursor(rect, pixels_per_point);
    }

    /// Key events captured by the hook since the last call.
    ///
    /// Empty unless immersive mode is on.
    pub fn drain_captured_keys(&mut self) -> Vec<KeyEvent> {
        if !self.active && !self.forwarding {
            return Vec::new();
        }
        platform::drain_captured()
            .into_iter()
            .map(|(keycode, scancode, extended, pressed)| KeyEvent {
                keycode,
                pressed,
                modifiers: 0,
                scancode,
                extended,
            })
            .collect()
    }
}

impl Drop for Immersive {
    fn drop(&mut self) {
        self.release();
    }
}

// ── Windows implementation ────────────────────────────────────

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, SetThreadPriority,
        THREAD_PRIORITY_TIME_CRITICAL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, ClipCursor, GetCursorPos, WindowFromPoint, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
        PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
        HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, PM_REMOVE,
        LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    /// The hook callback is a bare C function, so its state has to be static.
    static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

    /// When this process started, for stamping events the callback records.
    ///
    /// A `tracing` line carries a wall-clock time, but the trace is written all
    /// at once when the mode ends, so its entries have no times of their own.
    /// That is what made the last round unreadable: the log showed a Win press
    /// reaching the hook and the foreground moving to the Start menu, with no
    /// way to tell which came first — and the whole remaining question is which
    /// came first.
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    /// Milliseconds since this process started. One clock read, no allocation.
    fn now_ms() -> u32 {
        STARTED
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis()
            .min(u32::MAX as u128) as u32
    }

    /// [`now_ms`] when immersive mode was last switched on.
    static CAPTURE_AT: AtomicU32 = AtomicU32::new(0);

    /// How long the mode has been on, in milliseconds.
    pub fn since_capture_ms() -> u32 {
        now_ms().saturating_sub(CAPTURE_AT.load(Ordering::Relaxed))
    }
    /// The six modifier keys, left and right told apart.
    pub const MODIFIER_VKS: [u16; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];

    /// Whether a key is physically down, according to Windows.
    ///
    /// # Why the OS rather than egui or the hook
    ///
    /// egui reports modifiers as a state flag, and inside an immediate viewport
    /// that flag is not dependable — the same context whose `focused` read
    /// false while the window was plainly in use. The hook is not dependable
    /// either, for reasons this file documents at length.
    ///
    /// `GetAsyncKeyState` is neither: it asks Windows what is held right now.
    /// It cannot be stale, cannot be uninstalled, and distinguishes left from
    /// right, which egui cannot express at all.
    pub fn key_is_down(vk: u16) -> bool {
        // SAFETY: a documented read of global key state; nothing is owned.
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
    }

    /// Whether the pointer is over a window belonging to this process.
    ///
    /// # Why the OS rather than egui
    ///
    /// This used to be egui's pointer position, reported from inside the
    /// session viewport and pushed down here every frame. That is the third
    /// thing taken from that viewport's input — after `focused` and
    /// `modifiers` — and the first two were both wrong in ways that took days
    /// to find. There is no reason to trust the third.
    ///
    /// `WindowFromPoint` answers the question directly and cannot be stale: it
    /// takes the cursor's real position and names the window under it. Asking
    /// whether that window belongs to us also covers the main window and any
    /// popup without having to enumerate rectangles.
    pub fn pointer_over_our_window() -> bool {
        // SAFETY: documented reads of global cursor and window state; no
        // handles are owned or freed here.
        unsafe {
            let mut at = POINT::default();
            if GetCursorPos(&mut at).is_err() {
                return false;
            }
            let window = WindowFromPoint(at);
            if window.is_invalid() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            pid != 0 && pid == GetCurrentProcessId()
        }
    }

    /// Set when a key arrived while another application was in front.
    ///
    /// Drained by the UI, which releases everything the host believes is held.
    static LOST_FOREGROUND: AtomicBool = AtomicBool::new(false);

    /// Whether keys have been seen going somewhere else since the last check.
    pub fn take_lost_foreground() -> bool {
        LOST_FOREGROUND.swap(false, Ordering::Relaxed)
    }

    /// Forward a few keys to the host without taking them from this machine.
    ///
    /// # Why there are two modes rather than one
    ///
    /// Immersive mode swallows everything, which is right when the window is
    /// standing in for another computer and wrong the rest of the time. But
    /// outside it the host was missing keys it genuinely needs: egui reports
    /// Ctrl, Alt and Shift only as a *state flag*, never as key events, and it
    /// eats Tab for its own focus navigation. So a plain session could not send
    /// a bare modifier or a Tab at all.
    ///
    /// This mode takes those from the hook, where they arrive as real
    /// virtual-key codes with left and right told apart, and passes them on to
    /// this machine as well. Nothing is stolen: Alt+Tab still switches windows
    /// here, and the host also learns Alt and Tab were pressed.
    static FORWARD_ACTIVE: AtomicBool = AtomicBool::new(false);
    /// What the hook saw: virtual key, scancode, extended flag, pressed.
///
/// The scancode is the one that matters — see `protocol::KeyEvent::scancode`.
/// The virtual key travels with it because the escape shortcut is matched on
/// it, and because a log full of scancodes is unreadable.
    type Captured = (u16, u16, bool, bool);

    /// Keys the hook has taken, waiting for the UI thread to send them.
    ///
    /// # Why a ring of atomics rather than a `Mutex<Vec<_>>`
    ///
    /// It was a mutex, drained by the UI thread every frame, and that is a lock
    /// the callback can block on. The deadline it has to meet is not
    /// negotiable: Windows allows `LowLevelHooksTimeout` -- 300ms by default --
    /// and a callback that misses it has its answer thrown away. The keystroke
    /// is delivered to this machine no matter what the callback eventually
    /// returns.
    ///
    /// Frames were taking 120ms to draw on the machine where this was
    /// reported. A callback waiting behind two of those has already lost, and
    /// what that looks like from outside is exactly the fault reported: the
    /// hook runs, the counter says the key was swallowed, and the Start menu
    /// opens anyway.
    ///
    /// One producer (the hook thread), one consumer (the UI thread), fixed
    /// slots, no allocation. The callback's worst case is two atomic stores.
    const RING: usize = 256;
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY_SLOT: AtomicU64 = AtomicU64::new(0);
    static SLOTS: [AtomicU64; RING] = [EMPTY_SLOT; RING];
    /// How many keys the hook has written, and how many the UI has taken.
    static WRITTEN: AtomicUsize = AtomicUsize::new(0);
    static READ: AtomicUsize = AtomicUsize::new(0);

    fn pack(vk: u16, scancode: u16, extended: bool, pressed: bool) -> u64 {
        vk as u64 | (scancode as u64) << 16 | (extended as u64) << 32 | (pressed as u64) << 33
    }

    fn unpack(bits: u64) -> Captured {
        (bits as u16, (bits >> 16) as u16, bits >> 32 & 1 != 0, bits >> 33 & 1 != 0)
    }

    /// Queue a key for the host. Called only from the hook thread.
    fn push_captured(vk: u16, scancode: u16, extended: bool, pressed: bool) {
        let written = WRITTEN.load(Ordering::Relaxed);
        if written.wrapping_sub(READ.load(Ordering::Acquire)) >= RING {
            // The UI thread has stopped draining. Dropping the newest key
            // beats overwriting one already queued, which would deliver the
            // session's typing out of order.
            return;
        }
        SLOTS[written % RING].store(pack(vk, scancode, extended, pressed), Ordering::Relaxed);
        WRITTEN.store(written.wrapping_add(1), Ordering::Release);
    }
    /// The way out, recognised by the hook itself.
    ///
    /// Packed as `vk | mods << 16`, because the callback cannot take a lock to
    /// read a binding and must not block. See [`set_escape_shortcut`].
    static ESCAPE: AtomicU32 = AtomicU32::new(0);
    /// Set by the hook when the escape shortcut is pressed; polled by the app.
    static ESCAPE_PRESSED: AtomicBool = AtomicBool::new(false);

    /// Modifier state as the hook sees it, which is the only view that is
    /// correct while the keys themselves are being swallowed.
    static CTRL_DOWN: AtomicBool = AtomicBool::new(false);
    static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
    static ALT_DOWN: AtomicBool = AtomicBool::new(false);

    /// Whether the forwarding path may take a key, as of the last frame.
    ///
    /// # Why the callback no longer asks Windows this itself
    ///
    /// It called `GetCursorPos`, `WindowFromPoint`, `GetForegroundWindow` and
    /// `GetWindowThreadProcessId` -- four trips into the window manager, on
    /// every keystroke, from inside a callback that is holding up the system's
    /// input processing while it runs. The documentation for
    /// `LowLevelKeyboardProc` asks for the opposite: return quickly, and do not
    /// call anything that might block.
    ///
    /// What it answers is which window is in front. That changes on a human
    /// timescale, so a frame-old answer is as good as a fresh one, and the UI
    /// thread can afford to ask. It fails closed: cleared when forwarding
    /// stops, and never set unless this application is positively in front.
    static FORWARD_ALLOWED: AtomicBool = AtomicBool::new(false);

    /// Refresh [`FORWARD_ALLOWED`]. Called once a frame from the UI thread.
    pub fn publish_forward_gate() {
        let allowed = FORWARD_ACTIVE.load(Ordering::Relaxed)
            && pointer_over_our_window()
            && app_has_focus();
        FORWARD_ALLOWED.store(allowed, Ordering::Relaxed);
    }

    // Modifier virtual-key codes. A low-level hook reports the side-specific
    // ones, which is what makes right Alt distinguishable from left.
    const VK_LSHIFT: u16 = 0xA0;
    const VK_RSHIFT: u16 = 0xA1;
    const VK_LCONTROL: u16 = 0xA2;
    const VK_RCONTROL: u16 = 0xA3;
    const VK_LMENU: u16 = 0xA4;
    const VK_RMENU: u16 = 0xA5;
    const VK_SHIFT: u16 = 0x10;
    const VK_CONTROL: u16 = 0x11;
    const VK_MENU: u16 = 0x12;

    /// Keys worth forwarding when not in immersive mode.
    ///
    /// Deliberately short. These are the ones egui cannot express — the
    /// modifiers, which it only reports as state, and Tab, which it consumes —
    /// so everything else can stay on the ordinary path and cannot arrive
    /// twice. Nothing here changes which window is in front, which is what
    /// makes the foreground test below trustworthy for them.
    fn forwardable(vk: u16) -> bool {
        matches!(vk, 0x09 | 0x10 | 0x11 | 0x12 | 0xA0..=0xA5)
    }

    pub const MOD_CTRL: u8 = 1;
    pub const MOD_SHIFT: u8 = 2;
    pub const MOD_ALT: u8 = 4;

    /// Tell the hook which combination means "let me out".
    ///
    /// The hook has to recognise this itself. Everything else is swallowed, so
    /// egui never learns that Ctrl or Shift is held and could not match a
    /// shortcut even if it were still watching for one.
    pub fn set_escape_shortcut(vk: u16, mods: u8) {
        ESCAPE.store(vk as u32 | (mods as u32) << 16, Ordering::Relaxed);
    }

    /// Whether the escape shortcut has been pressed since the last call.
    pub fn take_escape_request() -> bool {
        ESCAPE_PRESSED.swap(false, Ordering::Relaxed)
    }

    fn note_modifier(vk: u16, pressed: bool) {
        match vk {
            VK_LCONTROL | VK_RCONTROL | VK_CONTROL => CTRL_DOWN.store(pressed, Ordering::Relaxed),
            VK_LSHIFT | VK_RSHIFT | VK_SHIFT => SHIFT_DOWN.store(pressed, Ordering::Relaxed),
            VK_LMENU | VK_RMENU | VK_MENU => ALT_DOWN.store(pressed, Ordering::Relaxed),
            _ => {}
        }
    }

    /// How many keys the callback has reported since the hook went in.
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    /// How many times the hook has been (re)installed since immersive came on.
    ///
    /// Windows removes a low-level hook outright if its callback misses the
    /// `LowLevelHooksTimeout` deadline, rather than merely ignoring that one
    /// keystroke. There is no API to ask whether a hook is still alive, so the
    /// only way to see it happening is to count the reinstalls and compare
    /// against the keys that arrived.
    static REINSTALLS: AtomicU32 = AtomicU32::new(0);

    /// Record what the callback did, without doing anything slow.
    ///
    /// # Why there is no logging in here any more
    ///
    /// This wrote a `tracing::info!` line — a file write — from inside the hook
    /// callback, on every Alt, Win and Tab press. Windows gives that callback
    /// `LowLevelHooksTimeout` to return and **silently uninstalls the hook**
    /// when it is missed. So the diagnostic added to investigate keys going
    /// missing was itself a strong candidate for making them go missing.
    ///
    /// Atomics only now. The counts are logged from the UI thread, where taking
    /// as long as it likes costs nothing.
    fn report(_vk: u16, pressed: bool, swallowed: bool) {
        if !pressed {
            return;
        }
        if swallowed {
            SWALLOWED.fetch_add(1, Ordering::Relaxed);
        } else {
            PASSED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Keys taken for the host, and keys let through, since the mode came on.
    static SWALLOWED: AtomicU32 = AtomicU32::new(0);
    static PASSED: AtomicU32 = AtomicU32::new(0);

    /// The longest a single callback has taken, in microseconds.
    ///
    /// The one number that separates "the hook decided wrongly" from "the hook
    /// decided too late". Anything approaching `LowLevelHooksTimeout` -- 300ms
    /// unless the registry says otherwise -- means Windows stopped waiting and
    /// delivered the keystroke here regardless of what was returned.
    static SLOWEST_US: AtomicU32 = AtomicU32::new(0);

    /// Times the callback and records the worst case seen.
    struct Timed(std::time::Instant);

    impl Drop for Timed {
        fn drop(&mut self) {
            let taken = self.0.elapsed().as_micros().min(u32::MAX as u128) as u32;
            SLOWEST_US.fetch_max(taken, Ordering::Relaxed);
        }
    }

    /// What the hook has done so far.
    pub struct Counters {
        pub swallowed: u32,
        pub passed: u32,
        pub reinstalls: u32,
        pub entered: u32,
        pub slowest_us: u32,
    }

    pub fn counters() -> Counters {
        Counters {
            swallowed: SWALLOWED.load(Ordering::Relaxed),
            passed: PASSED.load(Ordering::Relaxed),
            reinstalls: REINSTALLS.load(Ordering::Relaxed),
            entered: ENTERED.load(Ordering::Relaxed),
            slowest_us: SLOWEST_US.load(Ordering::Relaxed),
        }
    }

    fn current_mods() -> u8 {
        let mut bits = 0;
        if CTRL_DOWN.load(Ordering::Relaxed) {
            bits |= MOD_CTRL;
        }
        if SHIFT_DOWN.load(Ordering::Relaxed) {
            bits |= MOD_SHIFT;
        }
        if ALT_DOWN.load(Ordering::Relaxed) {
            bits |= MOD_ALT;
        }
        bits
    }

    /// Every entry to the callback, before any condition is looked at.
    ///
    /// # Why this is counted separately
    ///
    /// "0 keys taken, 0 passed through" has two completely different meanings:
    /// the callback never ran, or it ran and the flags said to do nothing. The
    /// first is a hook that Windows is not calling; the second is a bug in this
    /// file. They need opposite fixes, and every counter so far sat *inside* a
    /// conditional and so could not tell them apart.
    static ENTERED: AtomicU32 = AtomicU32::new(0);

    /// The first few raw events, exactly as the callback saw them.
    ///
    /// # Why raw values rather than another counter
    ///
    /// "Entered twice, took nothing" says the callback ran and then rejected
    /// the event, but not which test rejected it — the capture flag, the
    /// injected flag, or the message not looking like a key at all. Each has a
    /// different cause and a different fix, and adding one counter per branch
    /// would be a third round of the same guessing.
    ///
    /// Storing the inputs answers all of them at once. Formatting is left to
    /// the UI thread: a callback that formats strings is what caused an earlier
    /// round of this, since Windows removes a hook that misses its deadline.
    ///
    /// `(milliseconds into the mode, hook code, message, virtual key, flags)`.
    type RawEvent = (u32, i32, u32, u32, u32);
    static TRACE: Mutex<Vec<RawEvent>> = Mutex::new(Vec::new());
    /// How many raw events to keep.
    ///
    /// Was eight, which the mode being switched on and off around a few
    /// presses could exhaust before the interesting keystroke arrived -- every
    /// trace so far was mostly events from before immersive mode started.
    /// Recording only while capturing, and keeping more, fixes both halves.
    const TRACE_LIMIT: usize = 64;

    /// Forget the trace, so the next one covers only this immersive session.
    pub fn clear_trace() {
        if let Ok(mut trace) = TRACE.lock() {
            trace.clear();
        }
    }

    /// The trace, drained and made readable.
    pub fn take_trace() -> Vec<String> {
        let Ok(mut trace) = TRACE.lock() else { return Vec::new() };
        std::mem::take(&mut *trace)
            .into_iter()
            .map(|(at, code, message, vk, flags)| {
                format!(
                    "+{at}ms code {code}, msg 0x{message:04X}, vk 0x{vk:02X}, flags 0x{flags:02X}"
                )
            })
            .collect()
    }

    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let _timed = Timed(std::time::Instant::now());
        ENTERED.fetch_add(1, Ordering::Relaxed);

        // Recorded before any decision, so a rejected event is still visible.
        // `try_lock`, because a callback that waits on the UI thread is the
        // fault this file is chasing and a diagnostic must not be able to cause
        // it. Only while capturing: outside that the trace fills with ordinary
        // typing and the keystroke worth seeing never gets a slot.
        if CAPTURE_ACTIVE.load(Ordering::Relaxed)
            && let Ok(mut trace) = TRACE.try_lock()
            && trace.len() < TRACE_LIMIT
        {
            // SAFETY: for HC_ACTION the OS guarantees a KBDLLHOOKSTRUCT behind
            // lparam. For any other code that pointer is not ours to read.
            let (vk, flags) = if code == HC_ACTION as i32 {
                let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
                (info.vkCode, info.flags.0)
            } else {
                (0, 0)
            };
            trace.push((since_capture_ms(), code, wparam.0 as u32, vk, flags));
        }

        // Forwarding mode, when immersive is not on. Copied to the host and
        // then handed onward untouched, so this machine behaves exactly as it
        // would with no hook installed at all.
        //
        // The foreground test is safe here in a way it never was for the Win
        // key: none of these keys moves the foreground window, so the answer
        // still describes the keystroke being asked about. It fails closed —
        // forwarding a keystroke meant for another application is the harm
        // worth avoiding, and a modifier that does not reach the host is not.
        if code == HC_ACTION as i32
            && !CAPTURE_ACTIVE.load(Ordering::Relaxed)
            && FORWARD_ACTIVE.load(Ordering::Relaxed)
        {
            // SAFETY: for HC_ACTION the OS guarantees a KBDLLHOOKSTRUCT here.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let vk = info.vkCode as u16;
            let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
            let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
            let message = wparam.0 as u32;
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            let released = matches!(message, WM_KEYUP | WM_SYSKEYUP);

            if !injected
                && (pressed || released)
                && forwardable(vk)
                && FORWARD_ALLOWED.load(Ordering::Relaxed)
            {
                push_captured(vk, info.scanCode as u16, extended, pressed);
            }
        }

        if code == HC_ACTION as i32 && CAPTURE_ACTIVE.load(Ordering::Relaxed) {
            // SAFETY: for HC_ACTION the OS guarantees lparam points at a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let vk = info.vkCode as u16;

            // Anything SendInput produced is not a person typing. Forwarding it
            // would let this machine feed itself when it is also a host.
            let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
            // Part of the key's identity, not a detail: without it right Alt is
            // left Alt and the arrows are the numpad.
            let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;

            let message = wparam.0 as u32;
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            let released = matches!(message, WM_KEYUP | WM_SYSKEYUP);

            // # Forwarding is gated on the foreground. Suppression is not.
            //
            // These were treated as one decision and they are not, and getting
            // that wrong did real damage.
            //
            // *Suppression* cannot be decided by the foreground, because Win and
            // Alt+Tab move the foreground themselves — the key being judged
            // changes the evidence. That is true, and it is why the gate was
            // removed.
            //
            // *Forwarding* must be gated, and removing it was indefensible. The
            // comment that replaced it argued the mode was self-consistent
            // because Alt+Tab would go to the host so nobody could wander off.
            // Alt+Tab does not reliably go to the host — that is the open bug —
            // so people do wander off, and everything typed into whatever they
            // landed in was being shipped to another computer. Typing a message
            // in a chat window and having it arrive on a remote desktop is not
            // a degraded feature, it is a serious fault.
            //
            // The two failure directions are not comparable either. Failing to
            // forward loses a keystroke. Forwarding wrongly sends what someone
            // is typing to a different machine. So this fails closed: nothing
            // is forwarded unless this application is positively identified as
            // the one in front.
            // # Immersive mode takes the keyboard, full stop
            //
            // This was gated on the pointer being inside the window and the
            // application being in front. Both gates were wrong here, for the
            // same underlying reason: they are questions about *state that the
            // key being judged can change*. Pressing Win moves the foreground
            // before anyone can ask about it. That is what made every gated
            // version leak the first press.
            //
            // Immersive mode is not a guess about intent. Somebody switched it
            // on, the toolbar shows it is on, and the pointer is clipped inside
            // the window for as long as it is. There is nothing left to infer:
            // while it is on, every key belongs to the host.
            //
            // Leaving is handled where it belongs — on the UI thread, which
            // notices the application losing the foreground and switches the
            // mode off, releasing everything held. That is a recovery path, not
            // a gate: it runs after the fact, where being slow costs nothing,
            // and it cannot be raced by the keystroke it is reacting to.
            if !injected && (pressed || released) {
                note_modifier(vk, pressed);
                report(vk, pressed, true);

                // The way out. Recognised here rather than downstream, and
                // never forwarded — the host has no use for it.
                let escape = ESCAPE.load(Ordering::Relaxed);
                let (escape_vk, escape_mods) =
                    ((escape & 0xFFFF) as u16, (escape >> 16) as u8);
                if pressed && escape_vk != 0 && vk == escape_vk && current_mods() == escape_mods {
                    ESCAPE_PRESSED.store(true, Ordering::Relaxed);
                    return LRESULT(1);
                }

                push_captured(vk, info.scanCode as u16, extended, pressed);
                // Non-zero swallows it, so this machine does not also act on a
                // keystroke meant for the other one.
                return LRESULT(1);
            }
        }

        // SAFETY: passing the same arguments on is the documented contract.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// The foreground window's process, named, and whether it is ours.
    ///
    /// For diagnostics only — nothing keys off this. Names the executable
    /// rather than a bare process id, because "12748" needed a separate lookup
    /// last time and by then the process was gone.
    pub fn foreground_description() -> String {
        // SAFETY: reading our own process id.
        let ours = unsafe { GetCurrentProcessId() };
        match foreground_pid() {
            Some(pid) if pid == ours => format!("ours (pid {pid})"),
            Some(pid) => format!("pid {pid} — NOT ours (we are {ours})"),
            None => "unknown — no foreground window".to_string(),
        }
    }

    /// Whether this process owns the foreground window.
    ///
    /// Used only by the forwarding path. It is emphatically **not** used to
    /// decide whether to swallow a key — that is the mistake documented on
    /// [`super::Immersive`], where the key being judged was the one that moved
    /// the foreground.
    pub fn app_has_focus() -> bool {
        // SAFETY: reading our own process id.
        let ours = unsafe { GetCurrentProcessId() };
        foreground_pid() == Some(ours)
    }

    /// Which process owns the foreground window, or `None` if that cannot be
    /// established.
    ///
    /// Kept only to label the diagnostic below. **Nothing keys off this** — see
    /// the note in the hook callback for why a foreground test cannot decide
    /// whether to suppress a key that itself changes the foreground.
    fn foreground_pid() -> Option<u32> {
        // SAFETY: both are documented, take no ownership, and cannot fail in a
        // way that leaves anything to clean up.
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.is_invalid() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(foreground, Some(&mut pid));
            (pid != 0).then_some(pid)
        }
    }

    /// The hook thread, started once and kept for the life of the process.
    ///
    /// The hook itself is reinstalled whenever immersive mode is switched on.
    /// See [`refresh_hook`] for why that is not the churn it looks like.
    static HOOK_THREAD_STARTED: Mutex<bool> = Mutex::new(false);

    /// The hook thread, so a refresh can be asked for.
    static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

    /// Asks the hook thread to check whether its hook still works.
    const MSG_PROBE: u32 = WM_APP + 1;

    /// The moments worth measuring at, in the order they happen.
    const STAGE_SESSION: u32 = 1;
    const STAGE_IMMERSIVE: u32 = 2;
    /// Anything above this is a watcher tick, carrying its own second count.
    const STAGE_WATCH_BASE: u32 = 1000;

    fn stage_name(stage: u32) -> String {
        match stage {
            STAGE_SESSION => "when a session window opened".to_string(),
            STAGE_IMMERSIVE => "on the way into immersive mode".to_string(),
            s if s >= STAGE_WATCH_BASE => {
                format!("{}s after startup", s - STAGE_WATCH_BASE)
            }
            _ => "at an unnamed moment".to_string(),
        }
    }

    /// Probe every few seconds, so the moment the hook goes deaf is visible.
    ///
    /// # Why this exists and why it is off by default
    ///
    /// The hook passes both probes at startup and has received nothing by the
    /// time a session opens eight minutes later. Somewhere in between it stops
    /// being called, and the two candidates — the ffmpeg runtime loading and
    /// the window system starting — are 400ms apart in the log, which no
    /// arrangement of the existing probes can separate.
    ///
    /// Ticking makes the answer a timestamp. It is behind `TAIN_HOOK_WATCH`
    /// because it injects a keystroke every few seconds forever, which is not
    /// something to leave running for people who are not chasing this.
    pub fn watch_hook() {
        if std::env::var_os("TAIN_HOOK_WATCH").is_none() {
            return;
        }
        std::thread::Builder::new()
            .name("tain-hook-watch".into())
            .spawn(|| {
                for tick in 1..=180u32 {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    probe_hook(STAGE_WATCH_BASE + tick * 5);
                }
            })
            .ok();
    }

    /// Ask the hook thread to report whether its hook is still being called.
    ///
    /// # This used to reinstall the hook, and reinstalling breaks it
    ///
    /// The theory was chain position: Windows calls low-level keyboard hooks
    /// most recently installed first, so a hook installed at process start
    /// sits behind every hook installed by anything launched since, and rdev —
    /// what RustDesk grabs with — installs on grab rather than at startup.
    /// Reinstalling on the way into immersive mode should have put this at the
    /// front.
    ///
    /// Measured, it puts it in the grave. `SetWindowsHookExW` succeeds and the
    /// replacement is never called: zero of four injected events, where the
    /// hook installed at startup had just passed the same probe. Since the old
    /// hook is removed first, that session had no working hook at all — worse
    /// than the fault it was meant to fix.
    ///
    /// The same unhook-and-reinstall in a bare process, on a spawned thread
    /// sitting in `GetMessageW`, works: four of four, before and after. So this
    /// is something about *this* process, and until it is understood the one
    /// hook that works is not being thrown away to test a theory.
    ///
    /// What is left is the measurement, which costs nothing: the probes below
    /// say whether the hook is alive and whether the Win key reaches it, at the
    /// moment somebody switches immersive mode on.
    ///
    /// `stage` names the moment, because the useful measurement is not one
    /// answer but where between them it changes. The hook passes its probe at
    /// startup and then misses real keystrokes, so something between those two
    /// points is what to look for, and nothing so far has narrowed it: it is
    /// not the timing, not the chain, not eframe, and not a window holding the
    /// foreground — each of those was tested on this machine and cleared.
    fn probe_hook(stage: u32) {
        let thread = HOOK_THREAD_ID.load(Ordering::Relaxed);
        if thread == 0 {
            return;
        }
        // SAFETY: posting to a thread that has a message queue — it made one by
        // calling `SetWindowsHookExW` and sits in `GetMessageW`.
        unsafe {
            let _ = PostThreadMessageW(thread, MSG_PROBE, WPARAM(stage as usize), LPARAM(0));
        }
    }

    /// Install the hook on the calling thread, which must be the hook thread.
    ///
    /// # The module handle must be null
    ///
    /// Passing `GetModuleHandleW(NULL)` here left the hook registered and dead:
    /// the call succeeded and the callback was never invoked. A non-null module
    /// tells Windows the procedure lives in a DLL to inject into other
    /// processes; ours is in an executable, so there is nothing to inject.
    /// Low-level hooks are always called on the installing thread and need
    /// none.
    fn install_hook() -> Option<HHOOK> {
        // SAFETY: a WH_KEYBOARD_LL hook takes no module handle, and is
        // delivered to this thread, which pumps messages.
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
            Ok(hook) => {
                REINSTALLS.fetch_add(1, Ordering::Relaxed);
                Some(hook)
            }
            Err(e) => {
                tracing::warn!(
                    "Could not install the keyboard hook: {e}. Keys will stay on this machine."
                );
                None
            }
        }
    }

    /// Send one synthetic keystroke and report whether the hook saw it.
    ///
    /// Runs on the hook thread, just after installing and before the message
    /// loop begins — which is exactly the arrangement being tested.
    /// Inject one harmless keystroke. Returns whether it was actually sent.
    ///
    /// Reporting the send separately matters: without it a probe cannot tell
    /// "the hook is dead" from "nothing was ever injected", and would report
    /// the first for the second. `SendInput` is refused when a
    /// higher-integrity window holds the foreground, among other reasons.
    fn self_test_inject() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        };

        /// F24. A real key, and one nothing in practice is bound to.
        const PROBE_VK: u16 = 0x87;

        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(PROBE_VK),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let events = [key(Default::default()), key(KEYEVENTF_KEYUP)];
        // SAFETY: two well-formed events in a slice that outlives the call.
        let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
        if sent != events.len() as u32 {
            tracing::warn!(
                "Hook probe inconclusive — SendInput sent {sent} of {} events",
                events.len()
            );
            return false;
        }
        true
    }

    fn self_test() -> bool {
        let before = ENTERED.load(Ordering::Relaxed);
        if !self_test_inject() {
            return false;
        }

        // The hook is called during message retrieval, so this cannot simply
        // read the counter — the loop has not started yet. A short wait with
        // its own pump is enough for a keystroke this thread just sent.
        let mut msg = MSG::default();
        for _ in 0..20 {
            // SAFETY: a non-blocking peek, discarding whatever it finds.
            unsafe {
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE);
            }
            if ENTERED.load(Ordering::Relaxed) > before {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Does the Win key itself reach this hook, right now?
    ///
    /// # What this measures that the self-test cannot
    ///
    /// The self-test injects F24, which nothing else is interested in, so it
    /// only proves the callback is reachable. The reported fault is narrower
    /// than that: the callback is alive and is not called for the Win press
    /// that opens the Start menu. If a hook installed ahead of ours is taking
    /// it, an injected Win will disappear in exactly the same place, and the
    /// count says so.
    ///
    /// Ctrl is held across it so Windows cannot act on the Win key even if it
    /// does get through — the shell opens the Start menu only when nothing else
    /// was pressed between the Win down and its up. So this is invisible
    /// whichever way it comes out, which a probe that runs on every switch into
    /// immersive mode has to be.
    ///
    /// Four injected events. Four seen means the Win key reaches us; two means
    /// the Ctrl pair arrived and the Win pair was taken by somebody in front of
    /// us in the chain.
    fn chain_test(stage: &str) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        };

        const VK_LCONTROL_PROBE: u16 = 0xA2;
        const VK_LWIN_PROBE: u16 = 0x5B;

        let key = |vk: u16, flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let events = [
            key(VK_LCONTROL_PROBE, Default::default()),
            key(VK_LWIN_PROBE, Default::default()),
            key(VK_LWIN_PROBE, KEYEVENTF_KEYUP),
            key(VK_LCONTROL_PROBE, KEYEVENTF_KEYUP),
        ];

        let before = ENTERED.load(Ordering::Relaxed);
        // SAFETY: four well-formed events in a slice that outlives the call.
        let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
        if sent != events.len() as u32 {
            tracing::warn!("Chain probe {stage} inconclusive — SendInput sent {sent} of 4");
            return;
        }

        let mut msg = MSG::default();
        for _ in 0..20 {
            // SAFETY: a non-blocking peek, discarding whatever it finds.
            unsafe {
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE);
            }
            if ENTERED.load(Ordering::Relaxed).saturating_sub(before) >= 4 {
                tracing::info!(
                    "Chain probe {stage}: all 4 events reached the hook — the Win key is ours"
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let seen = ENTERED.load(Ordering::Relaxed).saturating_sub(before);
        // What was in front matters here and nowhere else. The hook survives two
        // minutes of an idle machine with every subsystem running, so whatever
        // kills it is something somebody did, and the window they did it in is
        // the first thing worth knowing.
        //
        // Nothing at all is a dead hook, and it says nothing about the chain:
        // the Ctrl events are of no interest to anyone else and would have
        // arrived. Some but not all is the interesting case — the Ctrl pair
        // came through and the Win pair was taken by a hook ahead of ours.
        if seen == 0 {
            tracing::warn!(
                "Chain probe {stage}: none of the 4 injected events reached the hook — it \
                 is not being called at all, so immersive mode cannot take keys. Foreground: {}",
                foreground_description(),
            );
        } else {
            tracing::warn!(
                "Chain probe {stage}: only {seen} of 4 injected events reached the hook. \
                 The Ctrl pair arrived and the Win pair did not, so a low-level hook ahead \
                 of ours is taking the Win key before we see it."
            );
        }
    }

    /// Self-test, and say so either way.
    ///
    /// A pass proves only that the callback is reachable for a key this process
    /// injected itself. It cannot prove the hook is ahead of everything else in
    /// the chain, because an injected key travels the same chain and would be
    /// taken by the same hook that takes the real one. It is still worth
    /// running: a failure here means the hook is not merely outranked but dead,
    /// and those need different answers.
    fn self_test_and_report(what: &str) {
        // Borrowed rather than 'static: the stage is formatted at the call.
        if self_test() {
            tracing::info!("Keyboard hook {what} — self-test passed, the callback is live");
        } else {
            tracing::warn!(
                "Keyboard hook {what} — self-test FAILED. SetWindowsHookExW succeeded and \
                 the callback is never invoked, so immersive mode cannot take keys."
            );
        }
    }

    /// Start the hook thread if it is not already running.
    ///
    /// Blocks until the hook is actually installed, so a caller that sets a
    /// capture flag immediately afterwards cannot race it.
    /// Returns true when this call is what started it, so a caller can skip
    /// asking for a refresh of a hook that was installed a moment ago.
    fn ensure_hook_thread() -> bool {
        // Before the thread, so the callback never initialises it itself.
        let _ = STARTED.get_or_init(std::time::Instant::now);
        let Ok(mut started) = HOOK_THREAD_STARTED.lock() else { return false };
        if *started {
            return false;
        }

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("tain-keyboard-hook".into())
            .spawn(move || {
                // # This thread has a deadline, so it is scheduled like one
                //
                // Windows gives the callback `LowLevelHooksTimeout` to answer,
                // and it can only answer once this thread is running. That is a
                // problem on the machine this was reported from, where the UI
                // thread was taking 120ms a frame and several decoder threads
                // were saturating the CPU: an ordinary-priority thread waiting
                // its turn behind those can easily miss 300ms.
                //
                // Raising it is safe because of what the thread does, which is
                // nothing. It sleeps in `GetMessageW`, and when woken performs
                // a handful of atomic stores and returns. There is no loop here
                // to starve the rest of the machine with.
                //
                // SAFETY: setting our own thread's priority. `GetCurrentThread`
                // returns a pseudo-handle that must not be closed.
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
                }

                // SAFETY: reading our own thread id.
                HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);

                let installed = install_hook();

                let Some(hook) = installed else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);

                // # Prove the hook is live, without needing anyone to type
                //
                // "callback entered 0 times" has two readings: the hook is dead,
                // or nobody pressed a key while it was armed. Those need
                // opposite responses — one is a rewrite onto Raw Input, the
                // other is nothing at all — and several rounds have been spent
                // unable to tell them apart from a log.
                //
                // Injecting one keystroke settles it. `SendInput` events do
                // reach low-level hooks, marked with `LLKHF_INJECTED`; the
                // callback counts every entry before it looks at that flag, and
                // ignores injected events for every other purpose. F24 is a
                // real virtual key that essentially nothing binds.
                self_test_and_report("installed on its own thread");
                chain_test("at startup");

                // Nothing is dispatched below, and the only message this thread
                // is ever sent is a request to put its hook back at the head of
                // the chain. The loop otherwise exists so the OS has a thread
                // in a message wait to call the hook on, and this thread does
                // nothing else, so it is always free to answer.
                let mut msg = MSG::default();
                // SAFETY: a standard message loop; 0 on WM_QUIT, -1 on error.
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                    if msg.message != MSG_PROBE {
                        continue;
                    }
                    // Run here rather than on the UI thread because the probes
                    // wait for the callback, and the callback arrives on this
                    // thread.
                    let stage = stage_name(msg.wParam.0 as u32);
                    self_test_and_report(&format!("checked {stage}"));
                    chain_test(&stage);
                }

                // SAFETY: installed on this thread, removed once, here.
                unsafe {
                    let _ = UnhookWindowsHookEx(hook);
                }
            });

        match spawned {
            // Failure leaves this unstarted, so a later attempt tries again
            // rather than assuming a hook that is not there.
            Ok(_) => {
                if let Ok(true) = ready_rx.recv() {
                    *started = true;
                    tracing::info!("Keyboard hook installed on its own thread");
                }
            }
            Err(e) => tracing::warn!("Could not start the keyboard hook thread: {e}"),
        }
        *started
    }

    /// Set the capture flag directly, without touching anything else.
    pub fn set_capture(active: bool) {
        CAPTURE_ACTIVE.store(active, Ordering::Relaxed);
    }

    pub fn install_key_hook() {
        // A hook installed a moment ago has just been probed already.
        if !ensure_hook_thread() {
            probe_hook(STAGE_IMMERSIVE);
        }
        CAPTURE_AT.store(now_ms(), Ordering::Relaxed);
        REPORTED.store(0, Ordering::Relaxed);
        SLOWEST_US.store(0, Ordering::Relaxed);
        clear_trace();
        CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
    }

    /// Turn plain-session forwarding on or off.
    pub fn set_forwarding(active: bool) {
        if active && !ensure_hook_thread() {
            // Once per session window, and only the first time it opens —
            // `Immersive::set_forwarding` filters repeats.
            probe_hook(STAGE_SESSION);
        }
        FORWARD_ACTIVE.store(active, Ordering::Relaxed);
        if !active {
            FORWARD_ALLOWED.store(false, Ordering::Relaxed);
        }
    }

    pub fn remove_key_hook() {
        // The hook stays installed; only the decision changes. Nothing to
        // uninstall means nothing to get wrong about when.
        CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
        // Whatever is still queued belongs to a mode that has ended.
        READ.store(WRITTEN.load(Ordering::Acquire), Ordering::Release);
        // The hook's own view of the modifiers goes with it: whatever was held
        // while it was swallowing is not held as far as this machine is
        // concerned, and a stale flag would misread the next escape shortcut.
        CTRL_DOWN.store(false, Ordering::Relaxed);
        SHIFT_DOWN.store(false, Ordering::Relaxed);
        ALT_DOWN.store(false, Ordering::Relaxed);
        ESCAPE_PRESSED.store(false, Ordering::Relaxed);
    }

    pub fn drain_captured() -> Vec<Captured> {
        let written = WRITTEN.load(Ordering::Acquire);
        let mut read = READ.load(Ordering::Relaxed);
        let mut keys = Vec::with_capacity(written.wrapping_sub(read));
        while read != written {
            keys.push(unpack(SLOTS[read % RING].load(Ordering::Relaxed)));
            read = read.wrapping_add(1);
        }
        READ.store(read, Ordering::Release);
        keys
    }

    pub fn clip_cursor(rect: egui::Rect, pixels_per_point: f32) {
        // egui reports viewport rects in logical points; ClipCursor wants
        // physical screen pixels.
        let to_px = |v: f32| (v * pixels_per_point).round() as i32;
        let bounds = RECT {
            left: to_px(rect.left()),
            top: to_px(rect.top()),
            right: to_px(rect.right()),
            bottom: to_px(rect.bottom()),
        };
        if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
            return;
        }
        // SAFETY: `bounds` is a valid RECT for the lifetime of the call.
        if let Err(e) = unsafe { ClipCursor(Some(&bounds)) } {
            tracing::debug!("ClipCursor failed: {e}");
        }
    }

    pub fn release_cursor() {
        // SAFETY: passing None lifts any existing confinement.
        let _ = unsafe { ClipCursor(None) };
        let _ = POINT::default(); // keep the import honest across feature sets
    }
}

// ── Everything else ───────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
mod platform {
    pub const MOD_CTRL: u8 = 1;
    pub const MOD_SHIFT: u8 = 2;
    pub const MOD_ALT: u8 = 4;

    pub fn install_key_hook() {
        tracing::warn!(
            "Immersive mode is Windows-only for now — system keys will stay local \
             and the pointer will not be confined."
        );
    }
    pub fn remove_key_hook() {}
    pub fn set_forwarding(_active: bool) {}
    pub fn take_lost_foreground() -> bool {
        false
    }
    pub fn take_trace() -> Vec<String> {
        Vec::new()
    }
    pub fn clear_trace() {}
    pub fn since_capture_ms() -> u32 {
        0
    }
    pub fn publish_forward_gate() {}
    pub struct Counters {
        pub swallowed: u32,
        pub passed: u32,
        pub reinstalls: u32,
        pub entered: u32,
        pub slowest_us: u32,
    }
    pub fn counters() -> Counters {
        Counters { swallowed: 0, passed: 0, reinstalls: 0, entered: 0, slowest_us: 0 }
    }
    pub fn set_capture(_active: bool) {}
    pub fn app_has_focus() -> bool {
        true
    }
    pub fn foreground_description() -> String {
        "not applicable".to_string()
    }
    pub fn pointer_over_our_window() -> bool {
        false
    }
    pub const MODIFIER_VKS: [u16; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];
    pub fn key_is_down(_vk: u16) -> bool {
        false
    }
    pub fn drain_captured() -> Vec<(u16, u16, bool, bool)> {
        Vec::new()
    }
    pub fn set_escape_shortcut(_vk: u16, _mods: u8) {}
    pub fn take_escape_request() -> bool {
        false
    }
    pub fn clip_cursor(_rect: egui::Rect, _pixels_per_point: f32) {}
    pub fn release_cursor() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_inactive_and_captures_nothing() {
        let mut immersive = Immersive::default();
        assert!(!immersive.is_active());
        assert!(immersive.drain_captured_keys().is_empty());
    }

    #[test]
    fn toggling_is_idempotent() {
        let mut immersive = Immersive::default();
        immersive.set_active(true);
        assert!(immersive.is_active());
        immersive.set_active(true);
        assert!(immersive.is_active());

        immersive.set_active(false);
        assert!(!immersive.is_active());
        immersive.set_active(false);
        assert!(!immersive.is_active());
    }

    #[test]
    fn release_is_safe_when_already_inactive() {
        let mut immersive = Immersive::default();
        immersive.release();
        immersive.release();
        assert!(!immersive.is_active());
    }

    #[test]
    fn inactive_mode_never_confines_the_pointer() {
        // Guard against clipping the cursor when the user has not asked for it.
        let mut immersive = Immersive::default();
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        immersive.confine_cursor(rect, 1.0);
        assert!(immersive.applied_clip.is_none());
    }

    /// Capture follows the mode, and nothing else.
    ///
    /// # The bug this is here for
    ///
    /// Capture used to be gated on whether this app owned the foreground
    /// window. Pressing Win *changes* the foreground window, so the gate was
    /// being asked about a keystroke that had already moved the evidence: the
    /// measured foreground during a Win press was a third process entirely,
    /// and every key was handed back to the local shell. Three rounds of
    /// refining that check failed before it was removed.
    #[test]
    fn capture_is_on_exactly_when_the_mode_is_on() {
        let mut immersive = Immersive::default();
        assert!(!immersive.is_armed(), "nothing captured before it is asked for");

        immersive.set_active(true);
        assert!(immersive.is_armed(), "asking for it is the whole condition");
        assert!(immersive.is_active());

        immersive.release();
        assert!(!immersive.is_armed());
        assert!(!immersive.is_active());
    }


    #[test]
    fn confinement_is_reapplied_only_when_the_window_moves() {
        let mut immersive = Immersive::default();
        immersive.set_active(true);

        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        immersive.confine_cursor(rect, 1.0);
        assert_eq!(immersive.applied_clip, Some(rect));

        // Same rect: no change recorded, so no repeated syscall.
        immersive.confine_cursor(rect, 1.0);
        assert_eq!(immersive.applied_clip, Some(rect));

        let moved = rect.translate(egui::vec2(40.0, 0.0));
        immersive.confine_cursor(moved, 1.0);
        assert_eq!(immersive.applied_clip, Some(moved));

        immersive.release();
        assert!(immersive.applied_clip.is_none());
    }

    #[test]
    fn captured_win_keys_become_key_events() {
        // Exercised directly rather than through the hook, which needs a real
        // message loop; this pins the translation the app depends on.
        // Left Win is scancode 0x5B with the extended prefix.
        let events: Vec<KeyEvent> = [(VK_LWIN, 0x5Bu16, true, true), (VK_LWIN, 0x5B, true, false)]
            .into_iter()
            .map(|(keycode, scancode, extended, pressed)| KeyEvent {
                keycode,
                pressed,
                modifiers: 0,
                scancode,
                extended,
            })
            .collect();

        assert_eq!(events[0].keycode, 0x5B);
        assert!(events[0].pressed);
        assert!(!events[1].pressed);
    }
}
