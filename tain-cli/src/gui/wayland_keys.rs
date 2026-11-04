//! The keyboard as it is, on Wayland, for a session and for immersive mode.
//!
//! # Why egui is not enough
//!
//! See [`super::immersive`] for the long version. egui's keyboard is built for
//! editing text: no Super key, no left and right, modifiers only as a state flag.
//! On Windows a low-level hook supplies what it cannot. There is no such hook
//! here, because no Wayland client may see another's keys, and it does not need
//! one: this application only wants its own.
//!
//! # A second keyboard on the same connection
//!
//! winit keeps its `wl_keyboard` to itself. So this asks the same `wl_display`
//! for another, on an event queue of its own, read on a thread of its own, the way
//! `smithay-clipboard` shares that connection with winit. The compositor gives
//! every keyboard a client has the same events while one of its windows has the
//! focus, as hardware key codes, so it gets all of them and winit is undisturbed.
//!
//! # The desktop's own shortcuts
//!
//! Those are the compositor's until it is asked to give them up, which is what
//! `zwp_keyboard_shortcuts_inhibit_manager_v1` is: while immersive mode is on, the
//! Super key and Alt+Tab reach this keyboard rather than the desktop. GNOME asks
//! the person first, once, and Plasma, sway and Hyprland just do it. Where it is
//! missing, immersive mode still forwards every key the desktop lets through.
//!
//! # Nothing is swallowed
//!
//! The difference from Windows that the rest of the code has to know about: these
//! keys reach winit as well. So egui is told to look away while immersive mode is
//! on, rather than being blind because a hook took everything first.
//! See [`super::immersive::Immersive::keys_reach_egui_too`].
//!
//! # X11
//!
//! Nothing here runs, and a session uses egui's keyboard as it did. The pointer
//! is still confined, but the desktop keeps its shortcuts.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tain_core::input_portal::windows_key_from_evdev;
use wayland_client::backend::Backend;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as InhibitManager,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor},
};

pub const MOD_CTRL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_ALT: u8 = 4;

/// Left and right Shift, Ctrl and Alt, as virtual keys.
pub const MODIFIER_VKS: [u16; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];

/// How long the desktop may spend asking whether shortcuts can be taken.
///
/// GNOME's question is a window of its own, so the session loses the focus while
/// it is up, and that must not read as somebody having gone to another app.
const ASKING_FOR: Duration = Duration::from_secs(30);

/// What the listening thread and the interface share.
#[derive(Default)]
struct Keys {
    /// Whether this is running at all: a Wayland session, with a keyboard.
    live: bool,
    /// Key codes down, as far as this client has been told.
    held: BTreeSet<u32>,
    /// Whether one of this application's windows has the keyboard.
    focused: bool,
    /// Immersive mode: every key goes to the host, and shortcuts are watched for.
    capture: bool,
    /// A session is open, so Tab is copied to the host.
    forwarding: bool,
    /// `(virtual key, scancode, extended, pressed)`, oldest first.
    captured: Vec<(u16, u16, bool, bool)>,
    /// The combination that leaves immersive mode.
    escape: (u16, u8),
    escape_requested: bool,
    /// The other shortcuts kept while immersed, as `(tag, vk, mods)`.
    shortcuts: Vec<(u8, u16, u8)>,
    shortcuts_taken: u32,
    /// Keys pressed as a shortcut, whose release is no business of the host's.
    kept_back: BTreeSet<u32>,
    /// The focus went elsewhere with a session open, so held keys have no
    /// release coming.
    lost_focus: bool,

    // The parts of the connection the interface thread needs to reach.
    connection: Option<Connection>,
    queue: Option<QueueHandle<Listener>>,
    seat: Option<WlSeat>,
    manager: Option<InhibitManager>,
    surface: Option<WlSurface>,
    inhibitor: Option<Inhibitor>,
    /// When the desktop was asked, until it has answered.
    asked_at: Option<Instant>,
}

static KEYS: Mutex<Option<Keys>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Keys) -> T) -> T {
    let mut keys = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    f(keys.get_or_insert_with(Keys::default))
}

impl Keys {
    fn modifiers(&self) -> u8 {
        let down = |codes: [u32; 2]| codes.iter().any(|code| self.held.contains(code));
        let mut bits = 0;
        if down([29, 97]) {
            bits |= MOD_CTRL;
        }
        if down([42, 54]) {
            bits |= MOD_SHIFT;
        }
        if down([56, 100]) {
            bits |= MOD_ALT;
        }
        bits
    }

    /// One key going down or up. What the shortcuts are and which keys go to the
    /// host are decided here, on the thread that heard it, for the same reason as
    /// on Windows: the interface may not draw another frame until something does.
    fn key(&mut self, code: u32, pressed: bool) {
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        let Some((vk, scancode, extended)) = windows_key_from_evdev(code) else {
            tracing::trace!("No Windows key for Linux key {code}");
            return;
        };
        if self.capture {
            if pressed {
                let mods = self.modifiers();
                if self.escape.0 != 0 && (vk, mods) == self.escape {
                    self.escape_requested = true;
                    self.kept_back.insert(code);
                    return;
                }
                if let Some((tag, _, _)) =
                    self.shortcuts.iter().find(|(_, key, keep)| (*key, *keep) == (vk, mods))
                {
                    self.shortcuts_taken |= 1 << tag;
                    self.kept_back.insert(code);
                    return;
                }
            } else if self.kept_back.remove(&code) {
                return;
            }
            self.captured.push((vk, scancode, extended, pressed));
        } else if self.forwarding && vk == 0x09 {
            // egui consumes Tab for its own focus navigation and never passes it
            // on. Everything else is egui's to send outside immersive mode.
            self.captured.push((vk, scancode, extended, pressed));
        }
    }

    /// Ask for the desktop's shortcuts, for the window that has the keyboard.
    fn take_shortcuts(&mut self) {
        let (Some(manager), Some(surface), Some(seat), Some(queue)) =
            (&self.manager, &self.surface, &self.seat, &self.queue)
        else {
            return;
        };
        if let Some(old) = self.inhibitor.take() {
            old.destroy();
        }
        self.inhibitor = Some(manager.inhibit_shortcuts(surface, seat, queue, ()));
        self.asked_at = Some(Instant::now());
        self.flush();
    }

    fn give_shortcuts_back(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
            self.flush();
        }
        self.asked_at = None;
    }

    fn flush(&self) {
        if let Some(connection) = &self.connection {
            let _ = connection.flush();
        }
    }
}

/// The thread's side of the queue. Everything it hears goes straight into
/// [`KEYS`]; it keeps only what it needs to stop a keyboard being made twice.
struct Listener {
    keyboard: Option<WlKeyboard>,
}

impl Dispatch<WlRegistry, GlobalListContents> for Listener {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Listener {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event {
            let has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
            if has_keyboard && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(queue, ()));
            } else if !has_keyboard {
                state.keyboard = None;
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, keys, .. } => with(|state| {
                state.focused = true;
                state.held = keys
                    .chunks_exact(4)
                    .map(|bytes| u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .collect();
                // Another of this application's windows now has the keyboard,
                // and the desktop's shortcuts were given up for the last one.
                // The same window back again still has them.
                let moved = state.surface.as_ref() != Some(&surface);
                state.surface = Some(surface);
                if state.capture && moved {
                    state.take_shortcuts();
                }
            }),
            wl_keyboard::Event::Leave { .. } => with(|state| {
                state.focused = false;
                state.held.clear();
                state.kept_back.clear();
                state.lost_focus = true;
            }),
            wl_keyboard::Event::Key { key, state: WEnum::Value(pressed), .. } => {
                with(|state| state.key(key, pressed == wl_keyboard::KeyState::Pressed));
            }
            _ => {}
        }
    }
}

impl Dispatch<Inhibitor, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &Inhibitor,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let active = matches!(event, zwp_keyboard_shortcuts_inhibitor_v1::Event::Active);
        tracing::info!(
            "The desktop {} its keyboard shortcuts",
            if active { "has given up" } else { "has taken back" }
        );
        with(|state| state.asked_at = None);
    }
}

delegate_noop!(Listener: ignore InhibitManager);

/// Start listening, given the `wl_display` the window system is using.
///
/// # Safety
///
/// `display` has to be a live `wl_display` that stays connected for the life of
/// the process, which is what winit's is.
pub unsafe fn start(display: *mut std::ffi::c_void) {
    // SAFETY: as this function's own contract says.
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let connection = Connection::from_backend(backend);
    let (globals, mut queue) = match registry_queue_init::<Listener>(&connection) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("Could not look for a keyboard on this Wayland connection ({e}) — \
                            immersive mode cannot take the keyboard");
            return;
        }
    };
    let handle = queue.handle();
    let seat: Option<WlSeat> = globals.bind(&handle, 1..=7, ()).ok();
    if seat.is_none() {
        tracing::warn!("This Wayland desktop has no seat — immersive mode cannot take the keyboard");
        return;
    }
    let manager: Option<InhibitManager> = globals.bind(&handle, 1..=1, ()).ok();
    if manager.is_none() {
        tracing::info!(
            "This desktop keeps its own shortcuts: it does not offer to give them up, so \
             immersive mode forwards only the keys the desktop lets through"
        );
    }
    with(|state| {
        state.live = true;
        state.connection = Some(connection.clone());
        state.queue = Some(handle);
        state.seat = seat;
        state.manager = manager;
    });

    let started = std::thread::Builder::new().name("wayland-keys".into()).spawn(move || {
        let mut listener = Listener { keyboard: None };
        loop {
            if let Err(e) = queue.blocking_dispatch(&mut listener) {
                tracing::warn!("Stopped reading the keyboard: {e}");
                with(|state| state.live = false);
                return;
            }
        }
    });
    if let Err(e) = started {
        tracing::warn!("Could not start reading the keyboard: {e}");
        with(|state| state.live = false);
    }
}

// ── What `immersive` asks ─────────────────────────────────────

/// Whether keys are coming from here rather than from egui.
pub fn live() -> bool {
    with(|state| state.live)
}

/// Whether egui is seeing the same keys, which on Linux it always is.
pub fn keys_also_reach_egui() -> bool {
    live()
}

/// Immersive mode starts.
pub fn install_key_hook() {
    with(|state| {
        state.capture = true;
        state.captured.clear();
        state.kept_back.clear();
        state.take_shortcuts();
    });
}

/// Immersive mode ends.
pub fn remove_key_hook() {
    with(|state| {
        let was_on = state.capture;
        state.capture = false;
        state.captured.clear();
        state.kept_back.clear();
        state.give_shortcuts_back();
        // Whatever was forwarded as held has no release coming now.
        state.lost_focus |= was_on;
    });
}

pub fn set_forwarding(active: bool) {
    with(|state| state.forwarding = active);
}

pub fn take_lost_foreground() -> bool {
    with(|state| std::mem::take(&mut state.lost_focus))
}

pub fn take_trace() -> Vec<String> {
    Vec::new()
}

pub fn clear_trace() {}

pub fn release_keyboard_raw_input() {}

pub fn real_key_summary() -> String {
    "not applicable".to_string()
}

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

/// Used by the start-up order on Windows, where the hook is installed long
/// before it captures. Here there is nothing to install early.
pub fn set_capture(active: bool) {
    if !active {
        with(|state| state.capture = false);
    }
}

pub fn app_has_focus() -> bool {
    with(|state| {
        let asking = state.asked_at.is_some_and(|at| at.elapsed() < ASKING_FOR);
        state.focused || asking || !state.live
    })
}

pub fn foreground_description() -> String {
    if app_has_focus() { "this application".to_string() } else { "another application".to_string() }
}

pub fn pointer_over_our_window() -> bool {
    false
}

/// Whether a modifier is down, by its left or right virtual key.
pub fn key_is_down(vk: u16) -> bool {
    let code = match vk {
        0xA0 => 42,
        0xA1 => 54,
        0xA2 => 29,
        0xA3 => 97,
        0xA4 => 56,
        0xA5 => 100,
        _ => return false,
    };
    with(|state| state.held.contains(&code))
}

pub fn drain_captured() -> Vec<(u16, u16, bool, bool)> {
    with(|state| std::mem::take(&mut state.captured))
}

pub fn set_escape_shortcut(vk: u16, mods: u8) {
    with(|state| state.escape = (vk, mods));
}

pub fn take_escape_request() -> bool {
    with(|state| std::mem::take(&mut state.escape_requested))
}

pub fn set_hook_shortcuts(shortcuts: &[(u8, u16, u8)]) {
    with(|state| {
        state.shortcuts.clear();
        state.shortcuts.extend_from_slice(shortcuts);
    });
}

pub fn take_hook_shortcuts() -> u32 {
    with(|state| std::mem::take(&mut state.shortcuts_taken))
}

pub fn held_modifiers() -> u8 {
    with(|state| state.modifiers())
}

pub fn os_modifiers() -> u8 {
    held_modifiers()
}

/// The desktop confines the pointer, by the window's own command: see
/// `session_window`.
pub fn clip_cursor(_rect: egui::Rect, _pixels_per_point: f32) {}

pub fn release_cursor() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Letter A, left Ctrl and left Alt, as the desktop numbers them.
    const A: u32 = 30;
    const CTRL: u32 = 29;
    const ALT: u32 = 56;

    fn immersed() -> Keys {
        Keys { capture: true, escape: (0x7a, MOD_CTRL | MOD_ALT), ..Keys::default() }
    }

    #[test]
    fn immersed_every_key_goes_to_the_host_down_and_up() {
        let mut keys = immersed();
        keys.key(A, true);
        keys.key(A, false);
        keys.key(125, true); // the Super key, which egui has no name for
        assert_eq!(
            keys.captured,
            vec![(0x41, 0x1e, false, true), (0x41, 0x1e, false, false), (0x5b, 0x5b, true, true)]
        );
    }

    #[test]
    fn the_way_out_is_recognised_here_and_never_sent() {
        let mut keys = immersed();
        keys.key(CTRL, true);
        keys.key(ALT, true);
        keys.captured.clear();

        keys.key(87, true); // F11
        assert!(keys.escape_requested);
        assert!(keys.captured.is_empty(), "the host must not see the way out");

        keys.key(87, false);
        assert!(keys.captured.is_empty(), "nor the end of it");
    }

    #[test]
    fn a_shortcut_kept_for_this_machine_stays_here() {
        let mut keys = immersed();
        keys.shortcuts = vec![(3, 0x41, MOD_CTRL)];
        keys.key(CTRL, true);
        keys.captured.clear();
        keys.key(A, true);
        keys.key(A, false);
        assert_eq!(keys.shortcuts_taken, 1 << 3);
        assert!(keys.captured.is_empty());
    }

    #[test]
    fn the_same_key_without_its_modifiers_is_only_a_key() {
        let mut keys = immersed();
        keys.shortcuts = vec![(3, 0x41, MOD_CTRL)];
        keys.key(A, true);
        assert_eq!(keys.shortcuts_taken, 0);
        assert_eq!(keys.captured.len(), 1);
    }

    #[test]
    fn outside_immersive_only_tab_is_copied() {
        let mut keys = Keys { forwarding: true, ..Keys::default() };
        keys.key(A, true);
        assert!(keys.captured.is_empty());
        keys.key(15, true);
        assert_eq!(keys.captured, vec![(0x09, 0x0f, false, true)]);
    }

    #[test]
    fn modifiers_are_read_from_either_side() {
        let mut keys = Keys::default();
        keys.key(97, true); // right Ctrl
        keys.key(54, true); // right Shift
        assert_eq!(keys.modifiers(), MOD_CTRL | MOD_SHIFT);
        keys.key(97, false);
        assert_eq!(keys.modifiers(), MOD_SHIFT);
    }
}
