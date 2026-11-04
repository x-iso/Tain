//! KWin's own window list, window capture and input, for sharing windows on
//! Plasma the way Windows shares them.
//!
//! # Why not the portal
//!
//! The screen cast portal is the one way in that every desktop agrees on, and
//! for windows it falls short twice. Only the desktop knows what is open, so the
//! choosing happens in its dialog rather than in Tain's picker with pictures of
//! each window; and KDE's RemoteDesktop portal streams screens and nothing else
//! (`remotedesktop.cpp` makes streams for outputs, a workspace or a virtual
//! output), so a window shared through the portal cannot be controlled. See
//! `docs/linux.md`.
//!
//! KWin has its own protocols for exactly this, which its portal is built on:
//!
//! - `org_kde_plasma_window_management`: every window, with its title, program,
//!   state and where it is — what `window_capture` answers on Windows.
//! - `zkde_screencast_unstable_v1`: a PipeWire stream of one window, asked for
//!   by the window's id, with no dialog.
//! - `org_kde_kwin_fake_input`: pointer, keys and touch, in the desktop's own
//!   coordinates, which is what lets a click land in the window it was aimed at.
//!
//! # Who may use them
//!
//! KWin keeps these from any program that has not asked for them in its
//! `.desktop` file, as `X-KDE-Wayland-Interfaces`, and it finds that file by
//! matching the running executable against the first word of each `Exec=` line
//! — so the line has to name the binary by its full path. Spectacle and KDE's own
//! remote desktop server get them the same way. Tain's `.desktop` file asks for
//! all three; `docs/linux.md` says how to install it so that the match holds.
//! Where it does not, nothing here starts, and window sharing falls back to the
//! portal, choosing in the desktop's dialog and watching only.
//!
//! On a Plasma newer than 6.5 the check is gone for anything not in a sandbox,
//! and all of this simply works.
//!
//! # A connection of its own
//!
//! Not the interface's. That one belongs to winit, and this lives in the core,
//! which also runs where there is no window at all. One more client of KWin's
//! costs nothing, and its events are read on a thread of its own.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tracing::*;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use wayland_protocols_plasma::plasma_window_management::client::{
    org_kde_plasma_window::{self, OrgKdePlasmaWindow},
    org_kde_plasma_window_management::{self, OrgKdePlasmaWindowManagement},
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};

/// The interfaces Tain's `.desktop` file has to ask for, in the form it asks.
pub const INTERFACES: &str =
    "org_kde_plasma_window_management,org_kde_kwin_fake_input,zkde_screencast_unstable_v1";

/// `org_kde_plasma_window_management` state flags.
const STATE_ACTIVE: u32 = 0x1;
const STATE_MINIMIZED: u32 = 0x2;
const STATE_SKIP_TASKBAR: u32 = 0x1000;

/// `zkde_screencast_unstable_v1` pointer mode: drawn into the picture, as the
/// portal path does. See `portal::select_sources`.
const POINTER_EMBEDDED: u32 = 2;

/// How long KWin may take to start a window's stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(3);

/// A window as KWin describes it.
#[derive(Debug, Clone, Default)]
pub struct Window {
    pub uuid: String,
    pub title: String,
    /// The program's id, `org.kde.dolphin` or `firefox`.
    pub app_id: String,
    pub pid: u32,
    states: u32,
    /// Where its frame is, in the desktop's logical coordinates, which is what
    /// the pointer is placed in. The stream is the same rectangle in pixels.
    pub geometry: (i32, i32, u32, u32),
    /// Owned by another window: a dialog or a popup, which the window it
    /// belongs to brings along.
    child: bool,
    /// Whether KWin has finished describing it.
    ready: bool,
}

impl Window {
    pub fn minimized(&self) -> bool {
        self.states & STATE_MINIMIZED != 0
    }

    pub fn active(&self) -> bool {
        self.states & STATE_ACTIVE != 0
    }

    /// Whether somebody could reasonably choose this one to share.
    ///
    /// The same idea as on Windows: something with a taskbar entry, on screen
    /// and with a name, and not this application or the desktop's own shell.
    pub fn offerable(&self) -> bool {
        self.ready
            && !self.child
            && !self.minimized()
            && self.states & STATE_SKIP_TASKBAR == 0
            && self.pid != std::process::id()
            && !self.title.trim().is_empty()
            && self.geometry.2 > 0
            && self.geometry.3 > 0
            && !matches!(self.app_id.as_str(), "org.kde.plasmashell" | "plasmashell" | "org.kde.kwin")
    }

    /// What the program is called, for a sentence: the last part of its id.
    pub fn app_name(&self) -> String {
        let id = self.app_id.trim_end_matches(".desktop");
        id.rsplit('.').next().unwrap_or(id).to_string()
    }
}

/// A window's id as the rest of Tain carries it: a number, where KWin has a
/// UUID. Stable for the window's life, never zero, and never one of the values
/// `window_capture` keeps for itself.
pub fn id_of(uuid: &str) -> u64 {
    // FNV-1a: stable across runs and builds, which a `DefaultHasher` is not
    // promised to be.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in uuid.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash >> 2) | 1
}

/// What the event thread knows, shared with everybody asking.
#[derive(Default)]
struct Table {
    windows: HashMap<u64, Window>,
    /// Each window's object, which is what it is brought to the front through.
    objects: HashMap<u64, OrgKdePlasmaWindow>,
}

/// A window stream KWin has been asked for: its PipeWire node once KWin says.
#[derive(Default)]
struct Started {
    answer: Mutex<Option<Result<u32, String>>>,
    told: Condvar,
}

/// The event thread's side of the queue.
struct Listener {
    table: Arc<Mutex<Table>>,
}

/// Everything here, once it is known to work.
pub struct Kwin {
    connection: Connection,
    queue: QueueHandle<Listener>,
    table: Arc<Mutex<Table>>,
    /// Kept so that it stays bound; everything it says arrives as events.
    _manager: OrgKdePlasmaWindowManagement,
    screencast: ZkdeScreencastUnstableV1,
    /// `None` where KWin's fake input is too old to place the pointer, which a
    /// window share needs. Windows can then be watched but not controlled.
    fake_input: Option<OrgKdeKwinFakeInput>,
}

/// KWin, if this is a Plasma session that lets Tain use it.
///
/// # Asked again after a refusal
///
/// KWin decides what a client may use when it connects, from the `.desktop`
/// entry Tain installs as it starts (see `tain-cli`'s `desktop_entry`). On the
/// very first run that entry may not have reached Plasma's application database
/// by the time Tain asks, and a refusal kept for the life of the process would
/// make the first run share windows through the portal for no lasting reason.
/// So a refusal is asked about again, on a thread of its own, at most every
/// [`RETRY`] — the interface never waits on it.
///
/// Logged once each way: whether window sharing can be what it is on Windows
/// is worth saying in the log of a machine whose owner is asking why not.
pub fn get() -> Option<&'static Kwin> {
    /// How long a refusal stands before KWin is asked again.
    const RETRY: Duration = Duration::from_secs(10);

    enum Attempt {
        Connected(&'static Kwin),
        /// Not a Plasma session on Wayland. Nothing will change that.
        NotPlasma,
        /// KWin said no, or did not answer, when last asked.
        Refused(std::time::Instant),
    }
    static ATTEMPT: Mutex<Option<Attempt>> = Mutex::new(None);
    static RETRYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn ask() -> Attempt {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_uppercase();
        if !desktop.contains("KDE") || std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Attempt::NotPlasma;
        }
        match Kwin::connect() {
            Ok(kwin) => Attempt::Connected(Box::leak(Box::new(kwin))),
            Err(e) => {
                if TOLD.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    debug!("Windows are still shared through the portal here: {e:#}");
                } else {
                    info!("Windows are shared through the portal here: {e:#}");
                }
                Attempt::Refused(std::time::Instant::now())
            }
        }
    }

    let mut attempt = ATTEMPT.lock().unwrap_or_else(|e| e.into_inner());
    match &*attempt {
        Some(Attempt::Connected(kwin)) => Some(*kwin),
        Some(Attempt::NotPlasma) => None,
        Some(Attempt::Refused(at)) => {
            if at.elapsed() >= RETRY && !RETRYING.swap(true, std::sync::atomic::Ordering::Relaxed) {
                std::thread::spawn(|| {
                    let answer = ask();
                    *ATTEMPT.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
                    RETRYING.store(false, std::sync::atomic::Ordering::Relaxed);
                });
            }
            None
        }
        // The first time, asked here and waited for: whoever is first is the
        // host starting up, on a thread of its own.
        None => {
            let answer = ask();
            let kwin = match &answer {
                Attempt::Connected(kwin) => Some(*kwin),
                _ => None,
            };
            *attempt = Some(answer);
            kwin
        }
    }
}

impl Kwin {
    fn connect() -> Result<Self> {
        let started = std::time::Instant::now();
        let connection = Connection::connect_to_env().context("could not connect to KWin")?;
        let (globals, mut queue) =
            registry_queue_init::<Listener>(&connection).context("KWin did not list its interfaces")?;
        let handle = queue.handle();
        debug!("KWin listed its interfaces after {:?}", started.elapsed());

        let offered = |name: &str| globals.contents().with_list(|list| list.iter().any(|g| g.interface == name));
        if !offered("org_kde_plasma_window_management") || !offered("zkde_screencast_unstable_v1") {
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            anyhow::bail!(
                "KWin keeps its window list and window capture from Tain. Tain's .desktop file has to \
                 say X-KDE-Wayland-Interfaces={INTERFACES} and start with Exec={exe} — see docs/linux.md"
            );
        }

        let table = Arc::new(Mutex::new(Table::default()));
        let manager: OrgKdePlasmaWindowManagement = globals
            .bind(&handle, 13..=16, ())
            .context("KWin's window list is too old to name windows by their id")?;
        let screencast: ZkdeScreencastUnstableV1 =
            globals.bind(&handle, 1..=4, ()).context("KWin would not stream a window")?;
        let fake_input: Option<OrgKdeKwinFakeInput> = match globals.bind(&handle, 4..=5, ()) {
            Ok(fake_input) => Some(fake_input),
            Err(e) => {
                info!("KWin's fake input is not available ({e}) — shared windows can be watched, not controlled");
                None
            }
        };
        if let Some(fake_input) = &fake_input {
            fake_input.authenticate(
                "Tain".to_string(),
                "Controlling a window shared with somebody connected to this computer".to_string(),
            );
        }

        // Two round trips: one for the windows, one for each window's own
        // description. A picker opened straight away then has something in it.
        let mut listener = Listener { table: Arc::clone(&table) };
        debug!(
            "Bound KWin's window list v{}, window streams v{} after {:?}",
            manager.version(),
            screencast.version(),
            started.elapsed()
        );
        queue.roundtrip(&mut listener).context("KWin did not answer")?;
        debug!(
            "KWin named {} window(s) after {:?}",
            table.lock().unwrap_or_else(|e| e.into_inner()).windows.len(),
            started.elapsed()
        );
        queue.roundtrip(&mut listener).context("KWin did not answer")?;
        debug!("KWin described them after {:?}", started.elapsed());

        std::thread::Builder::new()
            .name("kwin".into())
            .spawn(move || loop {
                if let Err(e) = queue.blocking_dispatch(&mut listener) {
                    warn!("Lost the connection to KWin: {e}");
                    return;
                }
            })
            .context("could not start reading from KWin")?;

        let count = table.lock().unwrap_or_else(|e| e.into_inner()).windows.len();
        info!(
            "Sharing windows through KWin: {count} window(s), {}",
            if fake_input.is_some() { "with control" } else { "to be watched only" }
        );
        Ok(Self { connection, queue: handle, table, _manager: manager, screencast, fake_input })
    }

    /// Every window KWin has described, offerable or not.
    pub fn windows(&self) -> Vec<(u64, Window)> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.windows.iter().filter(|(_, w)| w.ready).map(|(id, w)| (*id, w.clone())).collect()
    }

    pub fn window(&self, id: u64) -> Option<Window> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.windows.get(&id).filter(|w| w.ready).cloned()
    }

    /// Ask KWin for a PipeWire stream of one window, and wait for its node.
    pub fn stream_window(&self, id: u64) -> Result<WindowStream> {
        let uuid = self.window(id).map(|w| w.uuid).context("that window has closed")?;
        let started = Arc::new(Started::default());
        let stream = self.screencast.stream_window(uuid, POINTER_EMBEDDED, &self.queue, Arc::clone(&started));
        self.flush();

        let answer = {
            let answer = started.answer.lock().unwrap_or_else(|e| e.into_inner());
            let (mut answer, _) = started
                .told
                .wait_timeout_while(answer, STREAM_TIMEOUT, |answer| answer.is_none())
                .unwrap_or_else(|e| e.into_inner());
            answer.take()
        };
        // Closed again on the way out of every failure below, by being dropped.
        let mut started = WindowStream { stream, connection: self.connection.clone(), node: 0 };
        match answer {
            Some(Ok(node)) => {
                started.node = node;
                Ok(started)
            }
            Some(Err(why)) => anyhow::bail!("KWin would not stream that window: {why}"),
            None => anyhow::bail!("KWin did not start the window's stream in time"),
        }
    }

    /// Which of `ids` is at a point on the desktop: the one in front, where
    /// several overlap there, as far as this can tell — the active one first.
    pub fn window_at(&self, ids: &[u64], x: f64, y: f64) -> Option<u64> {
        let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut under: Vec<(u64, bool)> = ids
            .iter()
            .filter_map(|id| table.windows.get(id).map(|window| (*id, window)))
            .filter(|(_, window)| {
                let (left, top, width, height) = window.geometry;
                !window.minimized()
                    && x >= f64::from(left)
                    && y >= f64::from(top)
                    && x < f64::from(left) + f64::from(width)
                    && y < f64::from(top) + f64::from(height)
            })
            .map(|(id, window)| (id, window.active()))
            .collect();
        under.sort_by_key(|(_, active)| !active);
        under.first().map(|(id, _)| *id)
    }

    /// Bring a window to the front and give it the keyboard, unless it has both.
    ///
    /// # Why a click needs this and a Windows host does not
    ///
    /// It does need it on Windows too, and does without. A shared window's
    /// picture is the window itself, whole, even where another one covers it on
    /// the host — the compositor draws it on its own. So the guest sees the
    /// window and clicks it, and the click lands on whatever is on top at that
    /// point on the host's desk. Bringing the window forward first makes the
    /// click land where it was aimed, and puts keys after it in the window the
    /// guest is looking at.
    pub fn activate(&self, id: u64) {
        let object = {
            let table = self.table.lock().unwrap_or_else(|e| e.into_inner());
            if table.windows.get(&id).is_none_or(Window::active) {
                return;
            }
            table.objects.get(&id).cloned()
        };
        if let Some(object) = object {
            object.set_state(STATE_ACTIVE, STATE_ACTIVE);
            self.flush();
        }
    }

    /// The fake input device, where KWin lets Tain place the pointer.
    pub fn input(&self) -> Option<Input<'_>> {
        self.fake_input.as_ref().map(|device| Input { device, kwin: self })
    }

    fn flush(&self) {
        if let Err(e) = self.connection.flush() {
            debug!("Could not send to KWin: {e}");
        }
    }
}

/// One window's stream. Closed when dropped.
pub struct WindowStream {
    stream: ZkdeScreencastStreamUnstableV1,
    connection: Connection,
    /// The PipeWire node to read it from, on the user's own PipeWire daemon.
    pub node: u32,
}

impl Drop for WindowStream {
    fn drop(&mut self) {
        self.stream.close();
        let _ = self.connection.flush();
    }
}

/// Input through KWin, in the desktop's logical coordinates.
pub struct Input<'a> {
    device: &'a OrgKdeKwinFakeInput,
    kwin: &'a Kwin,
}

impl Input<'_> {
    pub fn pointer_to(&self, x: f64, y: f64) {
        self.device.pointer_motion_absolute(x, y);
        self.kwin.flush();
    }

    pub fn pointer_by(&self, dx: f64, dy: f64) {
        self.device.pointer_motion(dx, dy);
        self.kwin.flush();
    }

    /// A Linux button code, `BTN_LEFT` and so on.
    pub fn button(&self, button: u32, pressed: bool) {
        self.device.button(button, pressed as u32);
        self.kwin.flush();
    }

    /// `axis` 0 is vertical, 1 horizontal; `value` in the units a wheel's
    /// `wl_pointer.axis` uses, about 15 to a detent.
    pub fn axis(&self, axis: u32, value: f64) {
        self.device.axis(axis, value);
        self.kwin.flush();
    }

    /// A Linux key code.
    pub fn key(&self, code: u32, pressed: bool) {
        self.device.keyboard_key(code, pressed as u32);
        self.kwin.flush();
    }

    pub fn touch_down(&self, id: u32, x: f64, y: f64) {
        self.device.touch_down(id, x, y);
        self.device.touch_frame();
        self.kwin.flush();
    }

    pub fn touch_move(&self, id: u32, x: f64, y: f64) {
        self.device.touch_motion(id, x, y);
        self.device.touch_frame();
        self.kwin.flush();
    }

    pub fn touch_up(&self, id: u32) {
        self.device.touch_up(id);
        self.device.touch_frame();
        self.kwin.flush();
    }
}

// ── Events ────────────────────────────────────────────────────

impl Dispatch<WlRegistry, GlobalListContents> for Listener {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for Listener {
    fn event(
        _: &mut Self,
        manager: &OrgKdePlasmaWindowManagement,
        event: org_kde_plasma_window_management::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        trace!("KWin's window list: {event:?}");
        if let org_kde_plasma_window_management::Event::WindowWithUuid { uuid, .. } = event {
            let named = Named { id: id_of(&uuid), uuid: uuid.clone() };
            manager.get_window_by_uuid(uuid, queue, named);
        }
    }
}

/// What a window object was asked for by: KWin's UUID for it, which is what a
/// stream is asked for by, and the number the rest of Tain knows it as.
struct Named {
    id: u64,
    uuid: String,
}

impl Dispatch<OrgKdePlasmaWindow, Named> for Listener {
    fn event(
        state: &mut Self,
        window: &OrgKdePlasmaWindow,
        event: org_kde_plasma_window::Event,
        named: &Named,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use org_kde_plasma_window::Event;
        let mut table = state.table.lock().unwrap_or_else(|e| e.into_inner());
        if let Event::Unmapped = event {
            table.windows.remove(&named.id);
            table.objects.remove(&named.id);
            if window.version() >= 4 {
                window.destroy();
            }
            return;
        }
        table.objects.entry(named.id).or_insert_with(|| window.clone());
        let entry = table
            .windows
            .entry(named.id)
            .or_insert_with(|| Window { uuid: named.uuid.clone(), ..Window::default() });
        match event {
            Event::TitleChanged { title } => entry.title = title,
            Event::AppIdChanged { app_id } => entry.app_id = app_id,
            Event::StateChanged { flags } => entry.states = flags,
            Event::PidChanged { pid } => entry.pid = pid,
            Event::Geometry { x, y, width, height } => entry.geometry = (x, y, width, height),
            Event::ParentWindow { parent } => entry.child = parent.is_some(),
            Event::InitialState => entry.ready = true,
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, Arc<Started>> for Listener {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        started: &Arc<Started>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let answer = match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => Ok(node),
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => Err(error),
            zkde_screencast_stream_unstable_v1::Event::Closed => Err("the stream closed".to_string()),
            _ => return,
        };
        let mut slot = started.answer.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(answer);
            started.told.notify_all();
        }
    }
}

delegate_noop!(Listener: ignore ZkdeScreencastUnstableV1);
delegate_noop!(Listener: ignore OrgKdeKwinFakeInput);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_id_is_stable_and_never_a_reserved_value() {
        let uuid = "{4f9cbd2e-3c4b-4d5f-9d26-0d1f3a2b9c71}";
        assert_eq!(id_of(uuid), id_of(uuid));
        assert_ne!(id_of(uuid), id_of("{4f9cbd2e-3c4b-4d5f-9d26-0d1f3a2b9c72}"));
        for uuid in ["", "a", uuid] {
            let id = id_of(uuid);
            assert_ne!(id, 0);
            assert!(id < crate::window_capture::DESKTOP_CHOOSES);
        }
    }

    #[test]
    fn only_a_window_somebody_could_point_at_is_offered() {
        let window = Window {
            uuid: "x".into(),
            title: "Report.odt".into(),
            app_id: "org.kde.calligrawords".into(),
            pid: 1,
            states: 0,
            geometry: (10, 10, 800, 600),
            child: false,
            ready: true,
        };
        assert!(window.offerable());
        assert_eq!(window.app_name(), "calligrawords");
        assert!(!Window { states: STATE_MINIMIZED, ..window.clone() }.offerable());
        assert!(!Window { states: STATE_SKIP_TASKBAR, ..window.clone() }.offerable());
        assert!(!Window { child: true, ..window.clone() }.offerable());
        assert!(!Window { title: " ".into(), ..window.clone() }.offerable());
        assert!(!Window { pid: std::process::id(), ..window.clone() }.offerable());
        assert!(!Window { app_id: "org.kde.plasmashell".into(), ..window.clone() }.offerable());
        assert!(!Window { ready: false, ..window }.offerable());
    }
}
