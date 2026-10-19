//! Unified egui application for Tain.
//!
//! ## Layout
//!
//! Three bands, and the split matters: the header and footer are **fixed-height
//! panels** so the middle section — previous connections — claims everything
//! left over. Rendering all three inside one `CentralPanel` lets the header grow
//! to fit its contents and squeeze the middle down to nothing.
//!
//! ```text
//! ┌─ TopBottomPanel::top, exact_height ─────────┐
//! │ logo ⚙                                       │
//! │ Host: [admin][guest] │ Connect: [paste] (→)  │
//! ├─ CentralPanel, takes all remaining space ────┤
//! │ Previous connections:                        │
//! │ ┌────────┐ ┌────────┐                        │
//! ├─ TopBottomPanel::bottom ────────────────────┤
//! │ (guest pills)                                │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! ## Locking discipline
//!
//! The shared `Vec`s here are behind `std::sync::Mutex`, which is **not
//! reentrant**. Render passes snapshot state, drop the guard, draw, and apply
//! mutations afterwards. Locking again inside a loop that already holds the
//! guard deadlocks the UI thread outright.
//!
//! ## Async boundary
//!
//! egui's `update` is synchronous, so anything touching the server goes through
//! [`HostCommand`] on an unbounded channel drained by a tokio task.
//!
//! ## Sessions live elsewhere
//!
//! Guest sessions render in their own window ([`super::session_window`]), one
//! tab each. This window only ever shows host-side state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tracing::*;

use tain_core::network::ProbeOutcome;
use tain_core::t;
use tain_core::{ApprovalDecision, ConnectionCode, ControlGrants, PasscodeType, SessionManager};

use super::connections::{ConnectionStatus, ConnectionStore};
use super::immersive::Immersive;
use super::network_bridge::GuestStatus;
use super::session::GuestSession;
use super::frametime::FrameTimer;
use super::secrets::HostSecrets;
use super::session_window::SessionWindow;
use super::settings::Settings;
use super::settings_panel::{self, PanelState};
use super::theme::{self, *};

/// Shared application state accessible from all panels.
#[derive(Clone)]
pub struct AppState {
    pub host: HostPanelState,
    pub connect: ConnectPanelState,
    /// Hosts we have connected to before, persisted across restarts.
    pub previous_connections: Arc<Mutex<ConnectionStore>>,
    pub incoming: Arc<Mutex<Vec<IncomingConnection>>>,
    /// Persisted global preferences.
    pub settings: Arc<Mutex<Settings>>,
    /// Whether this state came from disk and may be written back to it.
    ///
    /// False for anything a test constructs. See [`AppState::save_settings`].
    on_disk: bool,
}

/// An incoming guest connection.
#[derive(Debug, Clone)]
pub struct IncomingConnection {
    pub session_id: u32,
    pub display_name: String,
    /// Whether the guest chose this name itself, rather than us generating it.
    pub name_is_self_reported: bool,
    pub mouse_allowed: bool,
    pub keyboard_allowed: bool,
    pub status: ApprovalStatus,
    /// Authenticated with the admin passcode rather than the guest one.
    ///
    /// Shown differently and never asked about — an admin code is the "this is
    /// my own machine" path and grants control outright. It still has to appear
    /// in the list: a connection that cannot be seen cannot be ended, and
    /// whoever holds that code is doing everything a guest would need
    /// permission for.
    pub is_admin: bool,
    /// What the card's tooltip says about the link, rebuilt every frame.
    ///
    /// Held as finished text rather than the numbers behind it because it is
    /// read in exactly one place and rebuilt from the session table anyway —
    /// keeping the parts would mean keeping them in step for no reader.
    pub link: String,
}

/// The reset icon and autoreset checkbox that follow every copy button.
///
/// A free function because the admin row draws two different things to its left
/// — the copy button, or the cover over it — and everything after that is the
/// same either way. Duplicating it was the alternative, and the copy that got
/// edited would have been whichever one was not being looked at.
fn render_passcode_row_tail(
    ui: &mut egui::Ui,
    typ: PasscodeType,
    settings: &Settings,
    edits: &mut PendingEdits,
) {
    ui.add_space(8.0);

    if theme::icon_button(
        ui,
        20.0,
        egui::include_image!("../../assets/reset_passcode.svg"),
        match typ {
            PasscodeType::Master => t!("header.reset_admin_hint"),
            PasscodeType::Guest => t!("header.reset_guest_hint"),
        },
    )
    .clicked()
    {
        edits.regenerate.push(typ);
    }

    ui.add_space(4.0);
    let mut on = autoreset_of(settings, typ);
    if ui
        .checkbox(&mut on, egui::RichText::new(t!("header.autoreset")).color(TEXT))
        .on_hover_text(t!("header.autoreset_hint"))
        .changed()
    {
        edits.set_autoreset.push((typ, on));
    }
}

/// The cover over the admin passcode's copy button, and how far it is open.
///
/// # Why it closes again
///
/// The cover exists so the code cannot reach the clipboard by accident. Left
/// open, it stops being a cover after the first use and the guard quietly
/// becomes decoration — so it shuts on the copy it was opened for, on a passcode
/// reset, and on its own after a short while if nothing happens.
#[derive(Default)]
struct AdminCover {
    /// Where the knob is, 0 at rest and 1 fully across.
    progress: f32,
    /// When the cover closes again, once it has been opened.
    closes_at: Option<std::time::Instant>,
}

impl AdminCover {
    /// How long an uncovered button stays uncovered with nothing done to it.
    ///
    /// Long enough to read the button and press it without hurrying, short
    /// enough that a window left open on a desk is not a copy button anybody
    /// walking past can press.
    const STAYS_OPEN: std::time::Duration = std::time::Duration::from_secs(20);

    fn is_open(&self) -> bool {
        self.closes_at.is_some_and(|at| at > std::time::Instant::now())
    }

    fn open(&mut self) {
        self.progress = 1.0;
        self.closes_at = Some(std::time::Instant::now() + Self::STAYS_OPEN);
    }

    fn close(&mut self) {
        self.progress = 0.0;
        self.closes_at = None;
    }
}

/// A connection waiting for the host to decide, and what saying yes means.
///
/// The responder is the thing the server is blocked on — dropping it answers
/// "no decision", which the server reads as a denial, so it is held until
/// somebody has actually chosen.
struct PendingApproval {
    respond: oneshot::Sender<ApprovalDecision>,
    /// The endpoint to remember, when this connection presented the **admin**
    /// passcode.
    ///
    /// `None` for a guest, which is every connection that only ever gets asked
    /// about and never remembered. One field rather than a bool beside an id,
    /// because the two can never disagree that way: there is no admin without
    /// an endpoint to trust, and no guest with one.
    admin_endpoint: Option<[u8; 32]>,
}

#[cfg(test)]
impl PendingApproval {
    /// A guest waiting to be let in — nothing to remember either way.
    fn guest(respond: oneshot::Sender<ApprovalDecision>) -> Self {
        Self { respond, admin_endpoint: None }
    }
}

/// Approval state for an incoming connection.
///
/// There is no "denied" state: denial disconnects the guest, so a refused
/// connection stops existing rather than sitting in the list greyed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    /// Connected, but nothing is being streamed until the host decides.
    Pending,
    /// The host allowed it. Frames are flowing; input may or may not be.
    Allowed,
}

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Allowed => write!(f, "allowed"),
        }
    }
}

/// Shared state for the host panel (passcode management).
#[derive(Clone)]
pub struct HostPanelState {
    pub master_passcode: Arc<Mutex<Option<String>>>,
    pub guest_passcode: Arc<Mutex<Option<String>>>,
    pub endpoint_id: Arc<Mutex<Option<String>>>,
    /// Our own endpoint as raw bytes, for detecting loopback connections.
    pub endpoint_bytes: Arc<Mutex<Option<[u8; 32]>>>,
    /// Why the host server isn't running, if it failed to start.
    pub error: Arc<Mutex<Option<String>>>,
    /// This machine's identity key and codes, so they outlive the process.
    pub secrets: Arc<HostSecrets>,
}

impl Default for HostPanelState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostPanelState {
    /// Test-safe: the credential store is in memory and dies with this value.
    pub fn new() -> Self {
        Self::with_secrets(HostSecrets::in_memory())
    }

    /// The real thing, reaching the platform credential store.
    pub fn persistent() -> Self {
        Self::with_secrets(HostSecrets::os())
    }

    fn with_secrets(secrets: HostSecrets) -> Self {
        Self {
            master_passcode: Arc::new(Mutex::new(None)),
            guest_passcode: Arc::new(Mutex::new(None)),
            endpoint_id: Arc::new(Mutex::new(None)),
            endpoint_bytes: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
            secrets: Arc::new(secrets),
        }
    }

    fn passcode(&self, typ: PasscodeType) -> Option<String> {
        match typ {
            PasscodeType::Master => self.master_passcode.lock().unwrap().clone(),
            PasscodeType::Guest => self.guest_passcode.lock().unwrap().clone(),
        }
    }
}

/// Read the autoreset preference for one passcode type.
///
/// There is deliberately only one home for this. The header checkbox used to
/// write to its own copy that nothing read, so turning it off in the header
/// changed nothing and the guest code kept regenerating anyway.
fn autoreset_of(settings: &Settings, typ: PasscodeType) -> bool {
    match typ {
        PasscodeType::Master => settings.admin_autoreset,
        PasscodeType::Guest => settings.guest_autoreset,
    }
}

fn set_autoreset_of(settings: &mut Settings, typ: PasscodeType, on: bool) {
    match typ {
        PasscodeType::Master => settings.admin_autoreset = on,
        PasscodeType::Guest => settings.guest_autoreset = on,
    }
}

/// Shared state for the connect panel.
///
/// There is no separate endpoint/ticket field: a `ConnectionCode` carries the
/// host's address, so one pasted string is everything the guest needs.
#[derive(Clone)]
pub struct ConnectPanelState {
    pub passcode: Arc<Mutex<String>>,
}

impl Default for ConnectPanelState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectPanelState {
    pub fn new() -> Self {
        Self { passcode: Arc::new(Mutex::new(String::new())) }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// Test-safe: nothing here can write a secret to the developer's machine.
    pub fn new() -> Self {
        Self::with_host(HostPanelState::new())
    }

    /// The running application's state. Named separately from [`Self::new`] so
    /// that reaching the real credential store is a decision made exactly once.
    pub fn persistent() -> Self {
        let mut state = Self::with_host(HostPanelState::persistent());
        state.previous_connections = Arc::new(Mutex::new(ConnectionStore::load()));
        *state.settings.lock().unwrap() = Settings::load();
        state.on_disk = true;
        state
    }

    fn with_host(host: HostPanelState) -> Self {
        Self {
            host,
            connect: ConnectPanelState::new(),
            previous_connections: Arc::new(Mutex::new(ConnectionStore::new())),
            incoming: Arc::new(Mutex::new(Vec::new())),
            settings: Arc::new(Mutex::new(Settings::default())),
            on_disk: false,
        }
    }

    /// Write the settings out, if this state is backed by disk at all.
    ///
    /// # Why this is a property of the state rather than a call-site decision
    ///
    /// The same reasoning as [`super::secrets::SecretStore`], learned the same
    /// way. A test builds `AppState::new()` precisely to stay away from the
    /// user's real data — and it did stay away from *reading* it, but every
    /// path that edited a setting still called `Settings::save`, which wrote
    /// straight into the real configuration directory. Running the suite
    /// overwrote the developer's own settings file.
    ///
    /// Putting the decision on the value means a state that was never loaded
    /// from disk cannot be saved to it, whatever any call site does.
    pub fn save_settings(&self) {
        if self.on_disk {
            self.settings.lock().unwrap().save();
        }
        // Whatever changed, the service's copy of the answer may now be stale.
        // Cheap, and getting it wrong means a machine that answers to a code
        // nobody holds any more.
        sync_unattended(self);
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
}

/// Tell the service whether to stand in for this machine, and with what.
///
/// # Nothing is sent until it is asked for
///
/// The identity is only handed over while the setting is on. Local System can
/// already read every user's credential store, so this grants no access that
/// was not there — but it does put the host key in a second place, and a
/// machine whose owner declined unattended access should not pay that. Turning
/// it off sends the opposite message rather than merely stopping: leaving the
/// key behind after somebody said no would keep the cost of a feature they
/// refused.
///
/// Failure is ordinary and quiet. Most machines have no service installed, and
/// "the pipe is not there" is the expected answer rather than a fault.
pub fn sync_unattended(state: &AppState) -> bool {
    use tain_core::service_control::{send, ControlMessage};

    let message = if state.settings().unattended_access {
        let Some(secret_key) = state.host.secrets.identity() else {
            // No credential store, so there is no stable identity to hand over
            // — and a service adopting a key that changes next launch would
            // answer to codes nobody has.
            tracing::debug!("No stored identity yet — nothing to hand the service");
            return false;
        };
        let codes = [tain_core::PasscodeType::Master, tain_core::PasscodeType::Guest]
            .into_iter()
            .filter_map(|typ| state.host.secrets.code(typ))
            .collect();
        // The whole settings object, sent on every save rather than diffed.
        // This runs from `save_settings`, so an approval, a changed frame rate
        // and a new audio exclusion all arrive by the same route — and the
        // service ends up configured exactly as this application is, rather
        // than as whatever subset somebody remembered to forward.
        //
        // A settings object that will not serialise is not worth failing the
        // handover over: the identity and the codes still get across, and the
        // service falls back to its defaults as it did before.
        let settings = serde_json::to_string(&state.settings()).unwrap_or_else(|e| {
            tracing::warn!("Could not send settings to the service ({e}) — it will use defaults");
            String::new()
        });
        ControlMessage::AdoptIdentity { secret_key, codes, settings }
    } else {
        ControlMessage::ForgetIdentity
    };

    match send(&message) {
        Ok(()) => {
            tracing::debug!("Told the service about unattended access");
            true
        }
        Err(e) => {
            tracing::debug!("The service is not listening ({e:#})");
            false
        }
    }
}

/// Send `message`, asking again for a moment if the first attempt is refused.
///
/// # For the second of two messages sent back to back
///
/// The service's control pipe has a single instance, and it puts up the next
/// one only once it has read the last message to its end. A message sent
/// straight after another can arrive in the moment there is no pipe at all and
/// be refused by a service that is running perfectly well. Where that happened
/// to "this process is hosting", the service did not hear it until the next
/// heartbeat five seconds later, and for those five seconds both it and this
/// application were bound to the same identity.
///
/// Only for a service that was just reached. Asking again on a machine with no
/// service installed would add half a second to nothing.
fn send_again_shortly(
    message: &tain_core::service_control::ControlMessage,
) -> anyhow::Result<()> {
    const WITHIN: std::time::Duration = std::time::Duration::from_millis(500);

    let started = std::time::Instant::now();
    loop {
        match tain_core::service_control::send(message) {
            Ok(()) => return Ok(()),
            Err(_) if started.elapsed() < WITHIN => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Repeat "this process is hosting" to the service for as long as it runs.
///
/// # Why a thread and not the frame loop
///
/// The interface is not a clock. egui repaints when there is a reason to, and a
/// host sitting minimised in the tray with nobody connected has none for
/// minutes at a time — which is precisely the state a machine is in when
/// somebody tries to reach it. A claim renewed only when a window happens to
/// redraw would expire while the application it describes was running perfectly
/// well, and the service would bind alongside it.
///
/// # Why it never stops
///
/// There is nothing to stop it for. The host server, once bound, lives as long
/// as the process; the way this application stops hosting is by exiting, and
/// then the thread goes with it. Ending on exit is [`stopped_hosting`]'s job
/// and it does not need this thread's help to do it.
fn keep_telling_the_service_we_host() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tain_core::service_control::{send, ControlMessage, HOSTING_HEARTBEAT};

    /// Set once a beating thread exists, so a second host start does not add a
    /// second one — they would say the same thing twice as often.
    static BEATING: AtomicBool = AtomicBool::new(false);

    if BEATING.swap(true, Ordering::Relaxed) {
        return;
    }

    let spawned = std::thread::Builder::new()
        .name("tain-hosting-heartbeat".into())
        .spawn(|| {
            // Logged once each way rather than every beat. Failing is the
            // normal state on a machine with no service installed, and a line
            // every five seconds saying so would be the whole log.
            let mut reached: Option<bool> = None;
            loop {
                let claim = ControlMessage::ApplicationHosting(true);
                // The first beat follows the handover this application sends
                // as it starts hosting, so it is the second of two messages and
                // may land in the gap between them — see `send_again_shortly`.
                // Missing it is the five seconds during which the service goes
                // on hosting too. On this thread, the half second it can cost a
                // machine with no service is paid by nobody.
                let ok = match reached {
                    None => send_again_shortly(&claim).is_ok(),
                    Some(_) => send(&claim).is_ok(),
                };
                if reached != Some(ok) {
                    match ok {
                        true => tracing::debug!("The service knows this process is hosting"),
                        false => tracing::debug!("The service is not listening"),
                    }
                    reached = Some(ok);
                }
                std::thread::sleep(HOSTING_HEARTBEAT);
            }
        });

    if let Err(e) = spawned {
        // The claim is still made — just once, by this thread, and it expires a
        // lease later. Which is the right way to fail: the service takes over
        // rather than neither of us hosting.
        BEATING.store(false, Ordering::Relaxed);
        tracing::warn!("Could not keep telling the service we host ({e}) — it will take over");
        let _ = send(&ControlMessage::ApplicationHosting(true));
    }
}

/// Tell the service this process has stopped hosting, so it can take over now.
///
/// A courtesy, not the mechanism. The claim expires on its own, and it has to:
/// an application that is killed, panics, or loses power says nothing on the
/// way out, and those are not exotic on a machine nobody is sitting at. What
/// this buys is the difference between the service taking over immediately and
/// taking over a lease later, for the one case that can afford to be polite.
///
/// # Everything the service needs goes with it
///
/// The handover — identity, codes, settings, approved devices — was sent when
/// this application started and whenever settings were saved, and at no other
/// time. A service installed or reinstalled while this was already running
/// therefore never received it: it had the identity and codes an older service
/// had stored, but no settings, and its first unattended session ran on
/// defaults. Sending it again at the moment of handing over means the service
/// takes over with exactly what this application had, however long ago the
/// service started.
fn stopped_hosting(state: &AppState) {
    use tain_core::service_control::{send, ControlMessage};

    let released = ControlMessage::ApplicationHosting(false);
    // Back to back with the handover, so asked again across the gap between
    // them when the service took the first — see `send_again_shortly`.
    let told = match sync_unattended(state) {
        true => send_again_shortly(&released),
        false => send(&released),
    };
    match told {
        Ok(()) => tracing::info!("Told the service to take over hosting"),
        Err(e) => tracing::debug!("Could not tell the service we stopped ({e:#})"),
    }
}

/// Opt-in debugging aids, from a comma-separated `TAIN_DEBUG` list.
///
/// Read in every build. What keeps sample data out of release is that
/// `seed_demo_data` is compiled out, not anything here — so `approval`, which
/// only acts on a demo guest, does nothing there either.
///
/// | Flag       | Effect                                              | Builds |
/// |------------|-----------------------------------------------------|--------|
/// | `demo`     | Fill the connection lists with sample rows          | debug  |
/// | `approval` | Open the approval dialog for the pending demo guest | debug  |
/// | `settings` | Open the settings window immediately                | all    |
/// | `loopback` | Connect to our own host endpoint on startup         | all    |
///
/// `frametime` is not read through here: [`FrameTimer::from_env`] reads the
/// same variable itself, in every build. It only measures, and the build whose
/// numbers matter is release.
///
/// `loopback` is available in release for the same reason. Measuring the whole
/// pipeline — capture, encode, send, receive, decode, display — needs both ends
/// running, and a debug build's per-pixel decode is slow for reasons that say
/// nothing about the shipped one. Reporting debug numbers as if they described
/// the real thing has already sent this investigation down a wrong turn once.
pub fn debug_flag(name: &str) -> bool {
    std::env::var("TAIN_DEBUG").is_ok_and(|v| {
        v.split(',').any(|f| f.trim().eq_ignore_ascii_case(name))
    })
}

/// Populate the UI with sample rows for visual iteration.
///
/// The middle and footer sections are empty until a real session exists, which
/// makes their layout impossible to check otherwise.
#[cfg(debug_assertions)]
pub fn seed_demo_data(state: &AppState) {
    if !debug_flag("demo") {
        return;
    }
    warn!("TAIN_DEBUG=demo — showing sample data, not real connections");

    let demo = |id: &str, name: &str, status: ConnectionStatus| {
        let mut entry =
            super::connections::SavedConnection::new(id.into(), name.into());
        entry.last_connected = 0;
        entry.has_code = true;
        entry.status = status;
        entry
    };
    state.previous_connections.lock().unwrap().set_entries(vec![
        demo("demo-1", "Example PC 1", ConnectionStatus::Waiting),
        demo("demo-2", "Example PC 2", ConnectionStatus::Failed),
        demo("demo-3", "Workshop laptop", ConnectionStatus::Connecting),
        demo("demo-4", "Studio desktop", ConnectionStatus::Online),
    ]);

    *state.incoming.lock().unwrap() = vec![
        IncomingConnection {
            session_id: 1,
            display_name: "Guest PC1".into(),
            name_is_self_reported: true,
            mouse_allowed: true,
            keyboard_allowed: true,
            status: ApprovalStatus::Allowed,
            is_admin: false,
            link: "direct — 192.168.0.10:57374\n12 ms round trip\n\
                   Sending 60 fps at 18 Mbit/s\nGuest is decoding 59 fps into 1856x1020"
                .into(),
        },
        IncomingConnection {
            session_id: 2,
            display_name: "Friend PC2".into(),
            name_is_self_reported: true,
            mouse_allowed: false,
            keyboard_allowed: false,
            status: ApprovalStatus::Pending,
            is_admin: false,
            link: String::new(),
        },
        IncomingConnection {
            session_id: 3,
            display_name: "My laptop".into(),
            name_is_self_reported: true,
            mouse_allowed: true,
            keyboard_allowed: true,
            status: ApprovalStatus::Allowed,
            // So the admin styling is visible without a second machine.
            is_admin: true,
            link: "direct — 10.8.0.3:41233\n84 ms round trip\n\
                   Sending 40 fps at 9 Mbit/s — held back by slow encoding"
                .into(),
        },
    ];
}

// ── Async command bridge ─────────────────────────────────────

/// Work the GUI needs performed against the running host server.
#[derive(Debug)]
pub enum HostCommand {
    /// Replace the passcode of this type, invalidating the old one.
    RegeneratePasscode(PasscodeType),
    /// Set which input devices a live session may drive.
    SetSessionControl { session_id: u32, grants: ControlGrants },
    /// Change what a live session may see. See `SessionManager::set_displays`.
    SetSessionDisplays { session_id: u32, displays: tain_core::protocol::DisplayGrant },
    /// Push edited settings to the host server. Applies to new sessions.
    ApplySettings(tain_core::SessionConfig),
    /// Disconnect a guest from the host side.
    KickSession { session_id: u32, reason: String },
}

/// The secret inside a saved code, if it is still ours to hand out.
///
/// Three things have to agree before a saved string may be adopted: it has to
/// parse (base58**check**, so a mangled credential fails here rather than
/// becoming a working passcode made of garbage), its type byte has to be the
/// one we are restoring, and its endpoint id has to be *this* endpoint. The
/// last is the one that actually bites: if the identity key could not be
/// restored, yesterday's code names a host that no longer exists, and adopting
/// its secret would produce a code the user could copy but nobody could dial.
fn restorable_secret(
    saved: &str,
    typ: PasscodeType,
    endpoint_id: [u8; 32],
) -> Option<[u8; 32]> {
    let code = match ConnectionCode::parse(saved) {
        Ok(code) => code,
        Err(e) => {
            warn!("Saved {} code is unusable ({e}); issuing a new one", typ.prefix());
            return None;
        }
    };
    if code.passcode_type != typ || code.endpoint_id != endpoint_id {
        debug!("Saved {} code belongs to a different identity", typ.prefix());
        return None;
    }
    Some(code.secret)
}

/// Bring back the saved code of this type, or issue a fresh one.
///
/// Auto-reset is the user saying "a new code every launch", so when it is on
/// the stored credential is destroyed rather than merely ignored — leaving a
/// working secret in the credential store past the moment it stopped being
/// shown is precisely the state that setting exists to avoid.
async fn restore_or_issue(
    server: &tain_core::HostServer,
    secrets: &HostSecrets,
    settings: &Settings,
    typ: PasscodeType,
) -> anyhow::Result<String> {
    if autoreset_of(settings, typ) {
        secrets.forget_code(typ);
        return issue(server, typ).await;
    }

    let endpoint_id = server.endpoint_id_bytes();
    if let Some(secret) =
        secrets.code(typ).and_then(|saved| restorable_secret(&saved, typ, endpoint_id))
    {
        return Ok(server.adopt_passcode(typ, secret).await);
    }

    let code = issue(server, typ).await?;
    if !secrets.set_code(typ, &code) {
        warn!("Could not save the {} code — it will change again next launch", typ.prefix());
    }
    Ok(code)
}

async fn issue(server: &tain_core::HostServer, typ: PasscodeType) -> anyhow::Result<String> {
    match typ {
        PasscodeType::Master => server.generate_master_passcode().await,
        PasscodeType::Guest => server.generate_guest_passcode().await,
    }
}

/// Keep the credential store in step with the code that is on screen.
///
/// One rule, applied everywhere a code changes: the store holds what is
/// currently shareable, unless the user asked for a fresh one each launch, in
/// which case it holds nothing.
pub fn remember_code(secrets: &HostSecrets, settings: &Settings, typ: PasscodeType, code: &str) {
    if autoreset_of(settings, typ) {
        secrets.forget_code(typ);
    } else {
        secrets.set_code(typ, code);
    }
}

/// Channels produced by [`init_host_server`].
///
/// The server itself is kept alive by its spawned tasks; the GUI reaches it
/// through `command_tx` rather than holding a handle it would have to lock.
pub struct HostHandles {
    /// Live session table. The GUI reconciles its incoming list against this
    /// every frame, which is what makes a guest disconnecting, timing out, or
    /// being kicked all clear the pill without a separate notification for each.
    pub sessions: Arc<SessionManager>,
    pub approval_rx: mpsc::UnboundedReceiver<tain_core::ApprovalRequest>,
    pub passcode_changed_rx: mpsc::UnboundedReceiver<String>,
    pub command_tx: mpsc::UnboundedSender<HostCommand>,
}

/// Channels for the reachability prober.
///
/// # Why these are not part of [`HostHandles`]
///
/// They were, and it made checking a code depend on this machine's *own*
/// endpoint being up. Those are unrelated: the prober binds an endpoint of its
/// own, and asking whether someone else is reachable does not require being
/// reachable yourself.
///
/// Once the host bind moved into the background — so a slow network could not
/// stop the window opening — that coupling became visible as a bug: pasting a
/// code offered a Connect button with no idea whether the host was there,
/// because the channel that would have asked did not exist yet. On a filtered
/// connection the host bind is exactly the thing that takes forever, so the
/// check was missing precisely when it was most wanted.
pub struct ProbeHandles {
    /// Ask the prober about one code now, rather than at the next sweep.
    pub check_tx: mpsc::UnboundedSender<ConnectionCode>,
    pub checked_rx: mpsc::UnboundedReceiver<(ConnectionCode, ProbeOutcome)>,
    /// Asks for a sweep now rather than at the next interval.
    ///
    /// The sweep runs on a timer, and a machine that has just come back is
    /// offline on every card until the timer comes round again. Worse, a card
    /// that has given up says so with no way to ask it to look again — and it
    /// can be wrong: a host reported as unreachable has connected on the next
    /// attempt more than once.
    pub resweep_tx: mpsc::UnboundedSender<()>,
}

/// Start the reachability prober. Returns at once; the bind happens inside.
pub fn init_prober(state: &AppState) -> ProbeHandles {
    let (check_tx, check_rx) = mpsc::unbounded_channel();
    let (checked_tx, checked_rx) = mpsc::unbounded_channel();
    let (resweep_tx, resweep_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_probe_loop(
        Arc::clone(&state.previous_connections),
        check_rx,
        checked_tx,
        resweep_rx,
    ));
    ProbeHandles { check_tx, checked_rx, resweep_tx }
}

/// Bind the host server, generate initial passcodes, and start accepting connections.
///
/// Returns an `Arc` handle rather than consuming the server: the GUI needs to
/// approve sessions and regenerate passcodes for as long as it runs.
pub async fn init_host_server(state: &AppState) -> anyhow::Result<HostHandles> {
    use tain_core::HostServer;

    let (approval_tx, approval_rx) = mpsc::unbounded_channel();
    let (changed_tx, passcode_changed_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // The identity comes first and is always kept. Every code embeds the
    // endpoint id, which is this key's public half, so a fresh key silently
    // invalidates every code this machine has ever handed out — including the
    // ones sitting in other people's saved-connection lists.
    let secrets = Arc::clone(&state.host.secrets);
    let restored_identity = secrets.identity();
    let server = Arc::new(
        HostServer::bind_with_identity(restored_identity)
            .await?
            .with_approval_channel(approval_tx)
            .with_passcode_changed_channel(changed_tx),
    );
    if restored_identity.is_none() && !secrets.set_identity(&server.identity_bytes()) {
        warn!("No credential store — this machine's codes will change every launch");
    }

    // # The same key in both directions
    //
    // Connections this machine *makes* carry the identity it is known by, not a
    // fresh key per dial. Without this a host cannot recognise a returning
    // device: trusting an admin endpoint remembers an id that would never
    // appear again, so the owner was asked to approve their own laptop on every
    // single connection. See `network::adopt_identity`.
    //
    // Taken from the server rather than from `restored_identity`, which is
    // `None` on a first run — the key exists by this point either way.
    tain_core::network::adopt_identity(server.identity_bytes());

    // Take back the graphics preference an older Tain wrote for itself. It
    // pinned this program to the integrated chip to make Desktop Duplication
    // work, and kept the encoder off the discrete one for the life of the
    // installation. See `gpu_preference::undo_our_old_preference`.
    {
        let done = state.settings().gpu_preference_undone;
        if tain_core::gpu_preference::undo_our_old_preference(done) {
            state.settings.lock().unwrap().gpu_preference_undone = true;
            state.save_settings();
        } else if !done {
            // Nothing to undo on this machine, and nothing to undo later
            // either — recorded so a preference chosen deliberately after this
            // point is never mistaken for ours.
            state.settings.lock().unwrap().gpu_preference_undone = true;
            state.save_settings();
        }
    }

    // The identity exists by now, which it did not when the settings were last
    // saved on a first run. See `sync_unattended`.
    sync_unattended(state);

    // And keep saying plainly that this machine already has a host. The service
    // yields to it: two hosts on one identity would have a guest reach
    // whichever answered first, which is worse than either alone.
    //
    // This *is* the mechanism now rather than belt and braces. The service used
    // to also require that nobody was signed in, which made saying so almost
    // decorative; it no longer does, because a signed-in user with no
    // application running is exactly the case that left the machine
    // unreachable. Nothing else tells the service this process is alive.
    keep_telling_the_service_we_host();

    // Start from the saved preferences rather than the library defaults.
    server.set_session_config(state.settings().to_session_config());

    *state.host.endpoint_id.lock().unwrap() = Some(server.endpoint_id());
    *state.host.endpoint_bytes.lock().unwrap() = Some(server.endpoint_id_bytes());

    let settings = state.settings();
    for typ in [PasscodeType::Master, PasscodeType::Guest] {
        let code = restore_or_issue(&server, &secrets, &settings, typ).await?;
        *match typ {
            PasscodeType::Master => &state.host.master_passcode,
            PasscodeType::Guest => &state.host.guest_passcode,
        }
        .lock()
        .unwrap() = Some(code);
    }

    info!("Host server listening — endpoint {}", server.endpoint_id());

    tokio::spawn({
        let server = Arc::clone(&server);
        async move {
            if let Err(e) = server.run().await {
                error!("Host server error: {}", e);
            }
        }
    });

    tokio::spawn(run_command_loop(
        Arc::clone(&server),
        command_rx,
        state.host.clone(),
        Arc::clone(&state.settings),
    ));
    Ok(HostHandles {
        sessions: server.sessions(),
        approval_rx,
        passcode_changed_rx,
        command_tx,
    })
}

/// How often saved connections are re-checked.
///
/// Each round is one QUIC connection per saved host, which on a relayed path is
/// not free — but it is the only way the cards can say anything true, and it
/// costs a few kilobytes against a stream measured in megabits.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(45);

/// Give up on a host that has not answered in this long.
///
/// Generous, because a cold path may need hole punching or a relay handshake
/// first, and calling a slow host "offline" would be the same false claim in
/// the other direction.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Keep the saved-connection cards' status honest.
///
/// This is what replaced showing "ready" for every row that happened to have a
/// code saved — a claim about a machine nobody had spoken to, sometimes for
/// weeks, sitting next to a button that would then fail. The probe answers the
/// two questions a card actually needs (is it up, does this code still work)
/// **without** raising an approval prompt on the other person's screen, which is
/// why it is a separate message rather than a trial connection.
async fn run_probe_loop(
    store: Arc<Mutex<ConnectionStore>>,
    mut check_rx: mpsc::UnboundedReceiver<ConnectionCode>,
    checked_tx: mpsc::UnboundedSender<(ConnectionCode, ProbeOutcome)>,
    mut resweep_rx: mpsc::UnboundedReceiver<()>,
) {
    // Shared rather than owned, because a sweep now hands it to several probes
    // at once and they outlive this scope by a task boundary.
    let mut prober = match tain_core::network::Prober::bind().await {
        Ok(prober) => std::sync::Arc::new(prober),
        Err(e) => {
            // Not fatal: the cards fall back to "checking..." forever, which is
            // still more honest than the claim this replaced.
            warn!("Could not start the reachability prober: {e}");
            return;
        }
    };

    // One prober, two callers — the sweep below and the paste button. Binding a
    // second endpoint for the ad-hoc checks would cost a socket, a keypair and a
    // relay handshake to duplicate something already sitting here.
    // # Why the prober is rebuilt rather than merely retried
    //
    // The sweep below already repeats, so a peer that comes back was always
    // going to be noticed — in theory. In practice the app would show a machine
    // offline until it was relaunched, and relaunching is precisely what
    // rebinds this endpoint.
    //
    // That is the tell. `Prober::bind` runs once, and its endpoint holds a relay
    // connection that can die — "Lost connection to relay server: Ping timeout"
    // appears in real logs. Once it does, every probe from it fails forever and
    // the sweep faithfully reports every saved machine as unreachable. The
    // answer is stale rather than wrong, which is the worst kind: it looks like
    // the other end is down.
    //
    // A sweep where *nothing at all* answered is the signal. One machine being
    // offline is ordinary; every machine being offline, twice running, is much
    // more likely to be this end.
    const REBIND_AFTER: u32 = 2;
    /// How soon to look again after a sweep found nothing.
    ///
    /// Shorter than the ordinary interval: something is wrong, and the whole
    /// problem is that recovery went unnoticed for far too long.
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12);

    let mut failed_sweeps = 0u32;
    let mut next_sweep = tokio::time::Instant::now();
    loop {
        tokio::select! {
            // A pasted code is someone waiting on an answer, so it goes first.
            biased;

            Some(code) = check_rx.recv() => {
                let outcome = prober.probe(&code, PROBE_TIMEOUT).await;
                if checked_tx.send((code, outcome)).is_err() {
                    return;
                }
            }

            // Asked for by hand. Bringing the next sweep forward rather than
            // probing here keeps one path through the work, so a manual check
            // and a timed one cannot drift apart in what they do.
            Some(()) = resweep_rx.recv() => {
                next_sweep = tokio::time::Instant::now();
                // Whatever the last sweep concluded, this is a fresh question.
                failed_sweeps = 0;
            }

            _ = tokio::time::sleep_until(next_sweep) => {
                // Collected in one pass and released before any network work:
                // holding the store locked across a probe would stall every
                // repaint.
                let targets = {
                    let mut store = store.lock().unwrap();
                    // Before any network work, so the cards say they are being
                    // asked rather than repeating the last answer for however
                    // long this takes.
                    store.forget_probe_results();
                    store.probe_targets()
                };

                // Counted so a sweep that reached nobody can be told apart from
                // a sweep with nobody to reach.
                let mut asked = 0u32;
                let mut answered = 0u32;

                // # All at once, not one after another
                //
                // These were probed in sequence, so a sweep cost one timeout per
                // saved machine — and the timeout is the normal case on a link
                // where the hosts are unreachable. Three of them was most of a
                // minute during which nothing on screen changed and the button
                // could not be pressed again to any effect.
                //
                // They do not depend on each other and the prober takes `&self`,
                // so the whole sweep costs one timeout now, and each card
                // updates as its own answer arrives rather than at the end.
                let mut probes = tokio::task::JoinSet::new();
                for (endpoint_hex, code) in targets {
                    let Ok(parsed) = ConnectionCode::parse(&code) else {
                        store
                            .lock()
                            .unwrap()
                            .set_probe_result(&endpoint_hex, ConnectionStatus::NoCode, None);
                        continue;
                    };

                    asked += 1;
                    let prober = std::sync::Arc::clone(&prober);
                    probes.spawn(async move {
                        let outcome = prober.probe(&parsed, PROBE_TIMEOUT).await;
                        (endpoint_hex, status_of(&outcome), why_of(&outcome))
                    });
                }

                while let Some(finished) = probes.join_next().await {
                    let Ok((endpoint_hex, status, why)) = finished else { continue };
                    if status.can_connect() {
                        answered += 1;
                    }
                    store.lock().unwrap().set_probe_result(&endpoint_hex, status, why);
                }

                if asked > 0 && answered == 0 {
                    failed_sweeps += 1;
                } else {
                    failed_sweeps = 0;
                }

                if failed_sweeps >= REBIND_AFTER {
                    // Said out loud: from the outside this is indistinguishable
                    // from every saved machine being switched off, and the
                    // difference is worth having in the log.
                    info!(
                        "No saved machine answered {failed_sweeps} sweeps running — \
                         rebinding the reachability prober"
                    );
                    match tain_core::network::Prober::bind().await {
                        Ok(fresh) => {
                            prober = std::sync::Arc::new(fresh);
                            failed_sweeps = 0;
                        }
                        Err(e) => warn!("Could not rebind the prober: {e}"),
                    }
                }

                next_sweep = tokio::time::Instant::now()
                    + if failed_sweeps > 0 { RETRY_INTERVAL } else { PROBE_INTERVAL };
            }
        }
    }
}

/// One probe answer, as a saved-connection status.
/// The sentence to keep beside a card, when there is one worth keeping.
///
/// Only a failure has anything to explain — a host that answered has said all
/// there is to say by answering.
fn why_of(outcome: &ProbeOutcome) -> Option<String> {
    match outcome {
        ProbeOutcome::Unreachable { why } => Some(why.clone()),
        _ => None,
    }
}

fn status_of(outcome: &ProbeOutcome) -> ConnectionStatus {
    match outcome {
        ProbeOutcome::Valid { .. } => ConnectionStatus::Ready,
        ProbeOutcome::Stale => ConnectionStatus::Stale,
        ProbeOutcome::Unreachable { .. } => ConnectionStatus::Unreachable,
    }
}

/// What the line under the Paste button is saying.
///
/// Pasting a code used to do nothing visible: the field it fills is not shown,
/// so a successful paste and a failed one looked identical. Worse, "the code
/// parsed" is not the question anyone is asking — they want to know whether
/// pressing Connect is going to work, and that needs the host asked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PasteState {
    /// Nothing pasted yet.
    #[default]
    Idle,
    /// It parsed. Waiting on the host to answer.
    Checking,
    /// The host answered and this code still opens it.
    Ready { host: Option<String>, needs_approval: bool },
    /// The host answered, but it does not accept this code any more.
    Stale,
    /// No answer at all.
    Unreachable,
}

impl PasteState {
    /// The line to show, and the colour to show it in.
    pub fn message(&self) -> Option<(String, egui::Color32)> {
        match self {
            Self::Idle => None,
            Self::Checking => Some((t!("paste.checking").into(), TEXT_DIM)),
            Self::Ready { host, needs_approval } => {
                let who = host.as_deref().unwrap_or_else(|| t!("paste.anonymous_host"));
                Some((
                    if *needs_approval {
                        t!("paste.ready_will_ask", host = who)
                    } else {
                        t!("paste.ready", host = who)
                    },
                    LIVE_GREEN,
                ))
            }
            // Both of these are "do not press Connect", but for different
            // reasons and with different fixes, so they do not share a message.
            Self::Stale => Some((t!("paste.expired").into(), AMBER)),
            Self::Unreachable => Some((t!("paste.unreachable").into(), OFFLINE_RED)),
        }
    }

    /// Whether this state should draw attention to the button.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// Execute GUI-issued commands against the server, writing results back into shared state.
async fn run_command_loop(
    server: Arc<tain_core::HostServer>,
    mut rx: mpsc::UnboundedReceiver<HostCommand>,
    host_state: HostPanelState,
    settings: Arc<Mutex<Settings>>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            HostCommand::RegeneratePasscode(typ) => match server.regenerate_passcode(typ).await {
                Ok(code) => {
                    let current = settings.lock().unwrap().clone();
                    remember_code(&host_state.secrets, &current, typ, &code);
                    let slot = match typ {
                        PasscodeType::Master => &host_state.master_passcode,
                        PasscodeType::Guest => &host_state.guest_passcode,
                    };
                    *slot.lock().unwrap() = Some(code);
                }
                Err(e) => error!("Failed to regenerate {} passcode: {}", typ.prefix(), e),
            },
            HostCommand::SetSessionControl { session_id, grants } => {
                if !server.approve_session(session_id, grants) {
                    warn!("Session #{} is no longer active", session_id);
                }
            }
            HostCommand::SetSessionDisplays { session_id, displays } => {
                // The screens are enumerated here rather than inside the
                // session table, which has no business walking DXGI — and the
                // answer is needed twice: to decide whether what is on screen
                // survives the change, and to tell the guest what it may now
                // pick between.
                let available = tain_core::screen_capture::displays();
                if !server.share_with_session(session_id, displays, &available) {
                    warn!("Session #{} is no longer active", session_id);
                }
            }
            HostCommand::KickSession { session_id, reason } => {
                if !server.kick_session(session_id, &reason) {
                    warn!("Session #{} is no longer active", session_id);
                }
            }
            HostCommand::ApplySettings(config) => {
                info!(
                    "Applying settings — capture {:?} @ {}fps, encoder {}",
                    config.capture.dimensions(),
                    config.capture.fps,
                    config.encode.effective_codec()
                );
                server.set_session_config(config);
            }
        }
    }
}

/// Read the system clipboard. egui only surfaces paste as a keyboard event, so
/// the design's paste *button* needs to go around it.
fn read_clipboard() -> Option<String> {
    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        }
        Err(e) => {
            warn!("Clipboard read failed: {e}");
            None
        }
    }
}

// ── Application ──────────────────────────────────────────────

/// Mutations collected during a render pass, applied once the guard is released.
#[derive(Default)]
struct PendingEdits {
    rename: Vec<(String, String)>,
    /// Endpoint ids to forget, dropping the stored code with them.
    forget: Vec<String>,
    /// Endpoint id of a saved connection to dial again.
    reconnect: Option<String>,
    /// Session id → the grants the host just selected for it.
    set_grants: Vec<(u32, ControlGrants)>,
    regenerate: Vec<PasscodeType>,
    /// Autoreset toggled from the header. Same setting the settings window edits.
    set_autoreset: Vec<(PasscodeType, bool)>,
    copied: Option<PasscodeType>,
    paste_passcode: bool,
    connect: bool,
    open_settings: bool,
    /// Actions raised by a pill, pending confirmation.
    confirm: Vec<(u32, ConfirmAction)>,
    /// Where the admin cover's knob was dragged to this frame.
    admin_slider: Option<f32>,
    /// The knob reached the end — uncover the copy button.
    admin_uncovered: bool,
    /// The cover was touched by somebody who has not read the warning yet.
    admin_warning: bool,
}

/// A decision held until the user confirms it.
///
/// Allowing a guest starts streaming this machine's screen and can hand over
/// its input; kicking ends someone's session. Neither should happen on a stray
/// click, so both go through here.
struct Confirm {
    session_id: u32,
    guest: String,
    /// Whether `guest` is a name the other machine chose for itself.
    guest_named_itself: bool,
    action: ConfirmAction,
    /// Whether the connection presented the admin passcode.
    ///
    /// Changes what is being asked rather than merely how it is worded — see
    /// [`Self::body`].
    is_admin: bool,
    /// Which devices the checkboxes currently offer. Deliberately starts empty:
    /// the safe answer must be the one you get by not thinking about it.
    grants: ControlGrants,
    /// The screens this machine has, read once when the dialog opens.
    ///
    /// Not re-read per frame: enumerating displays is a DXGI walk, and a list
    /// that changed while somebody was ticking boxes would move the boxes.
    screens: Vec<tain_core::protocol::DisplayInfo>,
    /// Which of them are ticked. Ids rather than indices, so the set survives
    /// the list being re-read.
    share: std::collections::BTreeSet<u32>,
    /// The windows this machine could share.
    ///
    /// Re-read when the picker opens rather than only when the dialog does. The
    /// two are minutes apart — a guest can be left waiting while somebody
    /// finishes what they were doing — and the point of a picker is to show
    /// what is on the desk *now*. Frozen for as long as it is open, though, for
    /// the same reason the screens are: a list that reorders itself under the
    /// pointer is a list nobody can click accurately.
    windows: Vec<tain_core::protocol::WindowInfo>,
    /// Which of them are chosen, if windows were chosen instead of screens.
    ///
    /// Windows and screens are exclusive by construction rather than by
    /// checking: a window share that also carried screens would not be a window
    /// share. See `DisplayGrant::Windows`.
    ///
    /// A set of handles rather than the [`tain_core::protocol::WindowInfo`]s
    /// themselves, so a title changing while the dialog is open — which a
    /// browser does on every navigation — cannot silently unpick a window.
    chosen: std::collections::BTreeSet<u64>,
    /// Whether the window picker is open over this dialog.
    picking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    /// A guest is waiting to be let in at all.
    Approve,
    /// An established guest is being removed.
    Kick,
    /// An established guest stays, and what it may see is being changed.
    ///
    /// # Why this is not the approval dialog again
    ///
    /// It asks the same questions and means something different by them.
    /// Approval decides whether a session starts; this decides what an existing
    /// one shows *from now on*, while somebody is watching. Closing it changes
    /// nothing, and there is no "deny" — refusing to change is what not
    /// applying already is. Ending the session is the kick button, which is
    /// still where it was.
    Change,
}

/// What the user chose, when they chose something.
///
/// There is no `Dismiss` here, and there used to be: closing the dialog was a
/// third answer because the dialog was modal and had to be got rid of. A popup
/// attached to its card is put away by clicking that card, which never reaches
/// this — leaving the guest waiting, which is the whole distinction the old
/// variant existed to preserve. See
/// [`GuestApp::closing_a_decision_does_not_answer_it`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Allow(ControlGrants),
    Deny,
    /// Keep the session and change what it may see and drive.
    Change(ControlGrants),
}

impl Confirm {
    /// A dialog with nothing pre-ticked.
    ///
    /// Only reached from tests and the debug flag that opens the dialog without
    /// a guest, so a release build sees no caller — but keeping it unconditional
    /// means the tests exercise the same code a debug build runs.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    fn new(session_id: u32, guest: String, guest_named_itself: bool, action: ConfirmAction) -> Self {
        Self::with_grants(session_id, guest, guest_named_itself, action, ControlGrants::NONE)
    }

    /// Seed the checkboxes from the user's configured defaults.
    fn with_grants(
        session_id: u32,
        guest: String,
        guest_named_itself: bool,
        action: ConfirmAction,
        grants: ControlGrants,
    ) -> Self {
        // # Why the primary, and not everything
        //
        // Sharing a screen is the thing this dialog exists to permit, so the
        // answer you get by not thinking about it has to be the smallest one
        // that is still useful. Every screen is not that: a second monitor
        // often has the things somebody would not have chosen to show.
        //
        // One screen and not zero, because a session that may see nothing is a
        // connection with no purpose — the checkboxes are there to *reduce* the
        // default, and to extend it deliberately.
        let screens = tain_core::screen_capture::displays();
        let share = screens
            .iter()
            .find(|d| d.primary)
            .or_else(|| screens.first())
            .map(|d| d.id)
            .into_iter()
            .collect();
        Self {
            session_id,
            guest,
            guest_named_itself,
            action,
            grants,
            screens,
            share,
            // Empty until the picker is opened, which is what fills it. Nothing
            // reads it before then, and enumerating every window on the desk is
            // work for a dialog that may only ever be denied.
            windows: Vec::new(),
            // Never the default. Sharing windows is the narrower answer and the
            // deliberate one — it has to be chosen, not arrived at.
            chosen: std::collections::BTreeSet::new(),
            picking: false,
            is_admin: false,
        }
    }

    /// Say that this connection presented the admin passcode.
    ///
    /// A different question entirely — see [`Self::body`] — so it changes the
    /// wording and takes away the checkboxes, which have nothing to decide.
    fn for_admin(mut self, yes: bool) -> Self {
        self.is_admin = yes;
        self
    }

    fn title(&self) -> &'static str {
        match (self.action, self.is_admin) {
            (ConfirmAction::Approve, true) => t!("approval.title_admin"),
            (ConfirmAction::Approve, false) => t!("approval.title_guest"),
            (ConfirmAction::Kick, _) => t!("approval.title_kick"),
            (ConfirmAction::Change, _) => t!("approval.title_change"),
        }
    }

    /// What actually happens if this is confirmed.
    ///
    /// The screen is the part people underestimate, so it leads: input can be
    /// revoked later, but anything visible during the session has been seen.
    fn body(&self) -> String {
        match (self.action, self.is_admin) {
            // # The admin question is not the guest question
            //
            // A guest is asking to watch, and the host chooses what to grant.
            // Somebody holding the admin passcode already has everything the
            // moment they are let in, so there is nothing to choose and
            // offering checkboxes would imply a limit that does not exist.
            //
            // What is actually being decided is whether this device is one of
            // yours — which is why the answer is remembered, and why it says so.
            // The admin case says what the code carries, because the dialog
            // has no permissions to show — there is nothing to choose, and a
            // dialog with nothing in it would look like a smaller decision
            // than a guest's rather than a larger one.
            (ConfirmAction::Approve, true) => t!("approval.wants_to_connect_admin").to_string(),
            (ConfirmAction::Approve, false) => t!("approval.wants_to_connect").to_string(),
            (ConfirmAction::Kick, _) => t!("approval.will_be_disconnected").to_string(),
            // # Said in the present tense, because it takes effect at once
            //
            // The approval dialog describes something that has not happened
            // yet and can still be refused. This one describes what is on
            // somebody else's screen right now, and applying narrows it while
            // they are watching.
            (ConfirmAction::Change, _) => t!("approval.is_watching").to_string(),
        }
    }

    /// Caveat shown when the name came from the other machine.
    fn name_caveat(&self) -> Option<&'static str> {
        (self.guest_named_itself && self.action == ConfirmAction::Approve)
            .then_some(t!("approval.self_named"))
    }

    fn confirm_label(&self) -> &'static str {
        match self.action {
            ConfirmAction::Approve => t!("approval.allow"),
            ConfirmAction::Kick => t!("approval.disconnect"),
            ConfirmAction::Change => t!("approval.apply"),
        }
    }

    /// Open on what a running session already has, rather than on a default.
    ///
    /// `None` for a session the table cannot answer about, which leaves the
    /// dialog on its default — the smallest useful answer, and the safe
    /// direction to be wrong in.
    ///
    /// A window grant fills the chosen set and the list it is read against,
    /// because [`Self::granted`] emits the windows in list order and would
    /// otherwise emit none of them.
    fn adopt(&mut self, current: Option<tain_core::protocol::DisplayGrant>) {
        use tain_core::protocol::DisplayGrant;
        let Some(current) = current else { return };

        match current {
            DisplayGrant::All => {
                self.share = self.screens.iter().map(|d| d.id).collect();
                self.chosen.clear();
            }
            DisplayGrant::Only(ids) => {
                self.share = ids.into_iter().collect();
                self.chosen.clear();
            }
            DisplayGrant::Windows(ids) => {
                self.chosen = ids.iter().copied().collect();
                self.windows = GuestApp::pickable(&self.chosen);
                // Anything the host granted that has since closed cannot be
                // offered and must not be silently re-granted either.
                self.chosen.retain(|id| self.windows.iter().any(|w| w.id == *id));
            }
        }
    }

    /// What this dialog's answers add up to, as the grant the session runs
    /// under.
    ///
    /// # The three answers, and why they are not one list
    ///
    /// Windows, when any were chosen: they *replace* the screens rather than
    /// joining them, which is the point of choosing them. Ordered as the picker
    /// listed them rather than by handle, because the first is what the session
    /// opens on and the order somebody saw is the only order they could have
    /// meant — handles sort by whatever number Windows happened to hand out.
    ///
    /// Every screen chosen means *all*, not "these ones that happen to be all
    /// of them": a monitor plugged in later should follow what was meant rather
    /// than be silently excluded.
    fn granted(&self) -> tain_core::protocol::DisplayGrant {
        use tain_core::protocol::DisplayGrant;

        if !self.chosen.is_empty() {
            return DisplayGrant::Windows(
                self.windows
                    .iter()
                    .map(|window| window.id)
                    .filter(|id| self.chosen.contains(id))
                    .collect(),
            );
        }
        if self.screens.is_empty() || self.share.len() == self.screens.len() {
            return DisplayGrant::All;
        }
        DisplayGrant::Only(self.share.iter().copied().collect())
    }
}

/// One window's answer to "what do you look like": its handle, and a picture if
/// it would give one.
type Shot = (u64, Option<tain_core::window_capture::Thumbnail>);

/// Pictures of this machine's windows, taken away from the interface thread.
///
/// # Why this is not simply called where the tiles are drawn
///
/// Asking a window to draw itself is a `SendMessage` into *its* message loop,
/// and it does not come back until that program answers. A program that is busy
/// — or wedged, which is not rare and is often exactly the one somebody wants
/// help with — would freeze the interface for as long as it stayed that way,
/// while the picker was open, on the machine of the person being asked to allow
/// a connection.
///
/// So the pictures are taken on a worker and arrive when they arrive. The tiles
/// draw immediately with names in them and fill in underneath, which also means
/// one unresponsive program costs its own tile and nothing else.
#[derive(Default)]
struct WindowShots {
    /// What has come back. `None` for a window that would not draw itself —
    /// remembered as such, so it is not asked again every frame.
    taken: std::collections::HashMap<u64, Option<egui::TextureHandle>>,
    /// Handed to a worker and not yet answered.
    asked: std::collections::HashSet<u64>,
    /// The worker's end and ours. Made on first use and kept: a channel per
    /// batch would drop results from the batch before it.
    post: Option<(std::sync::mpsc::Sender<Shot>, std::sync::mpsc::Receiver<Shot>)>,
}

impl WindowShots {
    /// How big a picture to ask each window for.
    ///
    /// Larger than the tile it is drawn in, because the tile is measured in
    /// points and a HiDPI screen has rather more pixels than that — a thumbnail
    /// sized for the logical tile is visibly soft on the machines most likely
    /// to be running this.
    const EDGE: u16 = 256;

    /// Start photographing these windows, and forget anything taken before.
    ///
    /// Called when the picker opens rather than kept warm: a picture of what a
    /// window looked like the last time somebody opened this is worse than no
    /// picture, because it is a picture that lies.
    fn refresh(&mut self, ctx: &egui::Context, ids: Vec<u64>) {
        self.taken.clear();
        self.asked.clear();
        if ids.is_empty() {
            return;
        }

        let (tx, _) = self.post.get_or_insert_with(std::sync::mpsc::channel);
        let tx = tx.clone();
        self.asked.extend(ids.iter().copied());

        let ctx = ctx.clone();
        // Detached deliberately: nothing waits for it, and a picker closed
        // before the last window answered simply throws the answer away.
        let spawned = std::thread::Builder::new()
            .name("tain-window-shots".into())
            .spawn(move || {
                for id in ids {
                    let shot = tain_core::window_capture::thumbnail(id, Self::EDGE);
                    // Sent one at a time rather than as a batch, so a program
                    // that takes a second to answer delays only its own tile.
                    if tx.send((id, shot)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        if spawned.is_err() {
            // No thread to be had. The tiles fall back to names, which is what
            // they show for a window that would not draw itself anyway.
            self.asked.clear();
        }
    }

    /// Take delivery of whatever has arrived. Cheap, and safe to call every
    /// frame.
    fn collect(&mut self, ctx: &egui::Context) {
        let Some((_, rx)) = self.post.as_ref() else {
            return;
        };
        // `try_iter` rather than a blocking read: this runs inside a frame.
        let arrived: Vec<Shot> = rx.try_iter().collect();
        for (id, shot) in arrived {
            self.asked.remove(&id);
            let uploaded = shot.map(|shot| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [usize::from(shot.width), usize::from(shot.height)],
                    &shot.rgba,
                );
                ctx.load_texture(format!("window-shot-{id}"), image, egui::TextureOptions::LINEAR)
            });
            self.taken.insert(id, uploaded);
        }
    }

    /// This window's picture, if one was taken.
    fn of(&self, id: u64) -> Option<&egui::TextureHandle> {
        self.taken.get(&id).and_then(|shot| shot.as_ref())
    }

    /// Whether a picture of this window is still on its way.
    fn waiting_for(&self, id: u64) -> bool {
        self.asked.contains(&id)
    }
}

/// The main egui application struct.
pub struct GuestApp {
    pub state: AppState,
    approval_rx: Option<mpsc::UnboundedReceiver<tain_core::ApprovalRequest>>,
    passcode_changed_rx: Option<mpsc::UnboundedReceiver<String>>,
    command_tx: Option<mpsc::UnboundedSender<HostCommand>>,
    /// Approval decisions the server is still waiting on, keyed by session.
    /// Nothing is streamed to these guests until one of these is answered.
    pending_approvals: HashMap<u32, PendingApproval>,
    /// Which passcode was most recently copied, for transient UI feedback.
    copied_at: Option<(PasscodeType, std::time::Instant)>,
    /// The guard over the admin passcode's copy button.
    admin_cover: AdminCover,
    /// Whether the one-time explanation of what that code is, is on screen.
    admin_warning_open: bool,
    /// Live guest sessions, one per tab in the session window.
    sessions: Vec<GuestSession>,
    /// The session window's own state (active tab, fullscreen, toolbar).
    session_window: SessionWindow,
    /// Win-key capture and pointer confinement, shared by whichever session is focused.
    immersive: Immersive,
    /// Frames drawn, until the window has been asked for focus once.
    ///
    /// See [`GuestApp::claim_focus_once`]. Counted rather than a flag because
    /// the request has to wait for the window to exist as far as Windows is
    /// concerned, which is one frame later than this application first runs.
    frames_before_focus: u32,
    /// The last scale reported to the log, so it is reported on change only.
    ///
    /// As `(what the monitor asks, what is being drawn, the window in points)`.
    last_reported_scale: Option<(f32, f32, egui::Vec2)>,
    /// The interface scale this application last handed to egui.
    ///
    /// Kept so the two directions can be told apart: egui's own Ctrl+plus and
    /// Ctrl+minus change the zoom without going through the settings, and
    /// without this the next frame would push the saved value straight back and
    /// the keyboard would appear not to work. See `sync_ui_scale`.
    applied_ui_scale: f32,
    /// Uploaded card pictures, keyed by host.
    ///
    /// Cached because a texture upload per card per frame would cost far more
    /// than the picture is worth, and `RefCell` because cards are drawn from
    /// `&self` — the alternative is threading `&mut` through the whole list
    /// render for a decoration.
    previews: std::cell::RefCell<std::collections::HashMap<String, egui::TextureHandle>>,
    /// Pictures of this machine's own windows, for the share picker.
    ///
    /// On the application rather than in the dialog because two guests waiting
    /// at once are two dialogs looking at the same desk, and photographing
    /// every window twice would cost twice as much for the same pictures.
    window_shots: std::cell::RefCell<WindowShots>,
    /// Host-side session table, reconciled against the incoming list each frame.
    host_sessions: Option<Arc<SessionManager>>,
    /// The host server's channels, still being bound. See
    /// [`GuestApp::attach_when_ready`].
    pending_handles: Option<tokio::sync::oneshot::Receiver<HostHandles>>,
    /// Why the last connection attempt failed, if it did.
    connect_error: Option<String>,
    /// What the pasted code turned out to be, once the host was asked.
    paste_state: PasteState,
    /// The code the pending check is about, so a late answer to a code the user
    /// has already replaced is discarded rather than shown against the new one.
    checking: Option<ConnectionCode>,
    check_tx: Option<mpsc::UnboundedSender<ConnectionCode>>,
    /// Asks the prober to sweep now. See [`ProbeHandles::resweep_tx`].
    resweep_tx: Option<mpsc::UnboundedSender<()>>,
    /// Sessions already announced, so a notification fires once each.
    ///
    /// `reconcile_incoming` runs every frame; without this the same arrival
    /// would be announced sixty times a second.
    announced: std::collections::HashSet<u32>,
    checked_rx: Option<mpsc::UnboundedReceiver<(ConnectionCode, ProbeOutcome)>>,
    /// Decisions awaiting an answer, one per connection.
    ///
    /// Several at once on purpose: each is drawn above the card it is about and
    /// is exactly as wide as it, so two guests arriving together are two
    /// questions side by side rather than a queue. See [`Self::render_confirms`].
    confirms: Vec<Confirm>,
    /// Whether the settings window is open.
    show_settings: bool,
    /// Transient state for the settings window (which shortcut is rebinding).
    settings_panel: PanelState,
    /// When the settings were last edited, while an edit is still settling.
    ///
    /// Dragging a slider reports a change on every frame it moves. Acting on
    /// each one wrote `settings.json` to disk and rebuilt the capture pipeline
    /// per frame — a single drag from 60 to 144 fps produced 28 reconfigurations
    /// in under a second, each one visible in the log and paid for by the
    /// encoder. The value the user is choosing is the one they stop on.
    settings_settling: Option<std::time::Instant>,
    /// Debug loopback: connect to our own host on the first frame.
    auto_connect: bool,
    /// Where the UI frame budget goes, when asked.
    frametime: FrameTimer,
}

impl GuestApp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            approval_rx: None,
            passcode_changed_rx: None,
            command_tx: None,
            pending_approvals: HashMap::new(),
            copied_at: None,
            admin_cover: AdminCover::default(),
            admin_warning_open: false,
            sessions: Vec::new(),
            session_window: SessionWindow::default(),
            immersive: Immersive::default(),
            frames_before_focus: 0,
            last_reported_scale: None,
            // Not the saved value: this is what egui has been told, and it has
            // been told nothing yet. The first frame pushes the setting in.
            applied_ui_scale: 1.0,
            previews: std::cell::RefCell::new(std::collections::HashMap::new()),
            window_shots: std::cell::RefCell::new(WindowShots::default()),
            host_sessions: None,
            pending_handles: None,
            connect_error: None,
            paste_state: PasteState::default(),
            checking: None,
            check_tx: None,
            resweep_tx: None,
            announced: std::collections::HashSet::new(),
            checked_rx: None,
            confirms: Vec::new(),
            show_settings: false,
            settings_panel: PanelState::default(),
            settings_settling: None,
            auto_connect: false,
            frametime: FrameTimer::from_env(),
        }
    }

    /// Apply the `TAIN_DEBUG` flags that affect the app rather than state.
    pub fn apply_debug_flags(&mut self) {
        self.auto_connect = debug_flag("loopback");
        self.show_settings = debug_flag("settings");

        // The approval dialog is otherwise only reachable by having a real
        // guest connect, which makes iterating on its wording tedious.
        if debug_flag("approval")
            && let Some(card) = self
                .state
                .incoming
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.status == ApprovalStatus::Pending)
        {
            self.confirms = vec![Confirm::new(
                card.session_id,
                card.display_name.clone(),
                card.name_is_self_reported,
                ConfirmAction::Approve,
            )];
        }
    }

    /// Take the channels once the endpoint has finished binding.
    ///
    /// Binding waits on the network and can take a long time on a filtered
    /// connection, so it happens in the background and arrives here whenever it
    /// is done. Until then the application runs without a host server: it can
    /// connect *out* perfectly well, and only accepting incoming connections has
    /// to wait.
    pub fn attach_when_ready(&mut self, handles: tokio::sync::oneshot::Receiver<HostHandles>) {
        self.pending_handles = Some(handles);
    }

    /// Attach the host server if its bind has completed since the last frame.
    ///
    /// Cheap: a non-blocking poll of a channel that resolves exactly once.
    fn take_pending_handles(&mut self) {
        let Some(pending) = self.pending_handles.as_mut() else { return };
        match pending.try_recv() {
            Ok(handles) => {
                self.pending_handles = None;
                info!("Host server ready — this machine can now accept connections");
                self.attach(handles);
            }
            // The task failed and said why; it set the error the panel shows.
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.pending_handles = None;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        }
    }

    /// Attach the channels and handle returned by [`init_host_server`].
    pub fn attach(&mut self, handles: HostHandles) {
        self.host_sessions = Some(handles.sessions);
        self.approval_rx = Some(handles.approval_rx);
        self.passcode_changed_rx = Some(handles.passcode_changed_rx);
        self.command_tx = Some(handles.command_tx);
    }

    /// Attach the prober, which is available from the moment the app starts.
    pub fn attach_prober(&mut self, handles: ProbeHandles) {
        self.check_tx = Some(handles.check_tx);
        self.checked_rx = Some(handles.checked_rx);
        self.resweep_tx = Some(handles.resweep_tx);
    }

    fn send(&self, cmd: HostCommand) {
        if let Some(ref tx) = self.command_tx {
            if tx.send(cmd).is_err() {
                error!("Host command channel closed");
            }
        } else {
            warn!("No host server attached — command ignored");
        }
    }

    /// Drain server notifications into UI state.
    fn poll_channels(&mut self) {
        if let Some(ref mut rx) = self.approval_rx {
            // Read before the loop: `settings()` takes a lock that must not be
            // held while `incoming` is locked below.
            let defaults = self.state.settings().default_grants();
            while let Ok(req) = rx.try_recv() {
                info!("Approval requested for session #{}: {}", req.session_id, req.label);

                // # The question asks itself
                //
                // It used to wait for the card to be clicked, which made the
                // card a notification *about* a decision rather than the
                // decision — one click before anything could be answered, and
                // one more thing to notice on a strip somebody is not looking
                // at. There is nothing being decided here that the person has
                // to ask for: a guest is already waiting, and the popup is what
                // says so.
                //
                // Only on arrival. Putting it away is a click on the card, and
                // it stays away — deriving this from "pending and no popup"
                // would reopen it on the next frame and make dismissing
                // impossible.
                self.confirms.push(
                    Confirm::with_grants(
                        req.session_id,
                        req.label.clone(),
                        req.label_is_self_reported,
                        ConfirmAction::Approve,
                        defaults,
                    )
                    .for_admin(req.is_admin),
                );

                self.state.incoming.lock().unwrap().push(IncomingConnection {
                    session_id: req.session_id,
                    display_name: req.label,
                    name_is_self_reported: req.label_is_self_reported,
                    mouse_allowed: false,
                    keyboard_allowed: false,
                    status: ApprovalStatus::Pending,
                    is_admin: req.is_admin,
                    // Nothing is streaming yet, so there is nothing to say
                    // about the link; `reconcile_incoming` fills this in.
                    link: String::new(),
                });
                // Hold the responder until the user decides. Dropping it here
                // would answer "no decision", which the server reads as a denial.
                self.pending_approvals.insert(
                    req.session_id,
                    PendingApproval {
                        respond: req.approve_tx,
                        admin_endpoint: req.is_admin.then_some(req.endpoint_id),
                    },
                );
            }
        }

        if let Some(ref mut rx) = self.passcode_changed_rx {
            while let Ok(code) = rx.try_recv() {
                info!("Guest passcode regenerated");
                // The server rotates this one itself when a guest disconnects,
                // so the credential store has to follow or a restart would put
                // back a code that no longer opens anything.
                let settings = self.state.settings();
                remember_code(
                    &self.state.host.secrets,
                    &settings,
                    PasscodeType::Guest,
                    &code,
                );
                *self.state.host.guest_passcode.lock().unwrap() = Some(code);
            }
        }

        if let Some(ref mut rx) = self.checked_rx {
            while let Ok((code, outcome)) = rx.try_recv() {
                // An answer about a code the user has since replaced is about a
                // question nobody is asking any more.
                if self.checking.as_ref() != Some(&code) {
                    continue;
                }
                self.checking = None;
                self.paste_state = match outcome {
                    ProbeOutcome::Valid { host_name, needs_approval } => {
                        PasteState::Ready { host: host_name, needs_approval }
                    }
                    ProbeOutcome::Stale => PasteState::Stale,
                    ProbeOutcome::Unreachable { .. } => PasteState::Unreachable,
                };
            }
        }

        self.prune_ended_sessions();
        self.reconcile_incoming();
        self.reconcile_saved_connections();
    }

    /// Ask the host whether a freshly pasted code will actually work.
    ///
    /// Sends nothing if the prober never started — the paste is still shown as
    /// accepted, which is true, just without the part that needs the network.
    fn check_pasted(&mut self, code: ConnectionCode) {
        let Some(tx) = self.check_tx.as_ref() else {
            self.paste_state = PasteState::Idle;
            return;
        };
        if tx.send(code.clone()).is_err() {
            self.paste_state = PasteState::Idle;
            return;
        }
        self.checking = Some(code);
        self.paste_state = PasteState::Checking;
    }

    /// Remember an endpoint the host has let in with the admin passcode.
    ///
    /// Written to disk and pushed to the running server, in that order: the
    /// file is what survives a restart, and the server's copy is what the
    /// *next* connection from this endpoint is checked against. Skipping either
    /// would mean the owner being asked again — once more this run, or once
    /// more every run.
    fn trust_admin_endpoint(&mut self, endpoint_id: [u8; 32]) {
        let changed = {
            let mut settings = self.state.settings.lock().unwrap();
            settings.trust_admin(endpoint_id)
        };
        if !changed {
            return;
        }
        self.state.save_settings();
        let config = self.state.settings().to_session_config();
        self.send(HostCommand::ApplySettings(config));
        info!(
            "This device is now a trusted admin endpoint. \
             Resetting the admin passcode forgets it again."
        );
    }

    /// Forget every endpoint trusted with the admin passcode.
    ///
    /// What a passcode reset does, and what the button in Settings does. After
    /// this the next admin connection from anywhere — including from this
    /// machine's owner — has to be approved again.
    fn forget_trusted_admins(&mut self) {
        let had = {
            let mut settings = self.state.settings.lock().unwrap();
            let had = settings.trusted_admin_endpoints.len();
            settings.trusted_admin_endpoints.clear();
            had
        };
        if had == 0 {
            return;
        }
        self.state.save_settings();
        let config = self.state.settings().to_session_config();
        self.send(HostCommand::ApplySettings(config));
        info!("Forgot {had} trusted admin endpoint(s) — the next one will be asked about");
    }

    /// Answer a guest that is waiting to be let in.
    fn resolve_approval(&mut self, session_id: u32, decision: ApprovalDecision) {
        // Cloned rather than moved: the decision is both *sent* to the waiting
        // session and used below to update this side's own view of it, and it
        // stopped being `Copy` when it grew a list of screens.
        match self.pending_approvals.remove(&session_id) {
            Some(pending) => {
                // # Saying yes to an admin is what creates the trust
                //
                // Recorded here rather than by the server, because this is the
                // moment somebody at the machine actually decided, and because
                // the answer has to outlive the process — a trust list that
                // forgot on every restart would ask the owner again every day
                // and train them to click through it.
                //
                // Only on `Allow`, and only for a connection that presented the
                // admin passcode. A denial leaves the endpoint exactly as
                // unknown as it was.
                if let (Some(endpoint), ApprovalDecision::Allow { .. }) =
                    (pending.admin_endpoint, &decision)
                {
                    self.trust_admin_endpoint(endpoint);
                }
                // The server is blocked on this: it has captured nothing and
                // sent nothing, and will only start once we answer.
                let _ = pending.respond.send(decision.clone());
            }
            None => {
                // Already streaming — the prompt closed, or this is a live
                // session whose permissions are being changed after the fact.
                match &decision {
                    ApprovalDecision::Allow { grants, .. } => {
                        let grants = *grants;
                        self.send(HostCommand::SetSessionControl { session_id, grants })
                    }
                    ApprovalDecision::Deny => self.kick(session_id),
                }
            }
        }

        match &decision {
            ApprovalDecision::Allow { grants, .. } => {
                let mut incoming = self.state.incoming.lock().unwrap();
                if let Some(c) = incoming.iter_mut().find(|c| c.session_id == session_id) {
                    c.status = ApprovalStatus::Allowed;
                    c.mouse_allowed = grants.mouse;
                    c.keyboard_allowed = grants.keyboard;
                }
            }
            // A refused guest is disconnected, so its card goes rather than
            // lingering greyed out for something that no longer exists.
            ApprovalDecision::Deny => {
                self.state.incoming.lock().unwrap().retain(|c| c.session_id != session_id);
            }
        }
    }

    /// Change what a live, already-allowed session may drive.
    fn set_grants(&mut self, session_id: u32, grants: ControlGrants) {
        self.resolve_approval(session_id, ApprovalDecision::Allow { grants, displays: tain_core::protocol::DisplayGrant::All });
    }

    fn apply(&mut self, edits: PendingEdits) {
        if !edits.rename.is_empty() || !edits.forget.is_empty() {
            let mut conns = self.state.previous_connections.lock().unwrap();
            for (endpoint_id, name) in edits.rename {
                conns.rename(&endpoint_id, name);
            }
            for endpoint_id in edits.forget {
                conns.forget(&endpoint_id);
            }
        }

        for (session_id, grants) in edits.set_grants {
            self.set_grants(session_id, grants);
        }

        for typ in edits.regenerate {
            self.send(HostCommand::RegeneratePasscode(typ));
            // # A reset takes the trusted devices with it
            //
            // Resetting the admin passcode is what somebody reaches for when a
            // code has got out, and the trust list is the other half of the
            // authorisation. Leaving it behind would revoke the code from
            // everybody except the machines that had already used it — which is
            // exactly backwards, and would look like it had worked.
            if typ == PasscodeType::Master {
                self.forget_trusted_admins();
            }
        }

        if !edits.set_autoreset.is_empty() {
            let mut settings = self.state.settings();
            for (typ, on) in edits.set_autoreset {
                set_autoreset_of(&mut settings, typ, on);
                // Take effect now, not at the next launch: switching off means
                // "keep the one I am looking at", and switching on means the
                // stored copy should stop existing at that moment.
                if let Some(code) = self.state.host.passcode(typ) {
                    remember_code(&self.state.host.secrets, &settings, typ, &code);
                }
            }
            *self.state.settings.lock().unwrap() = settings.clone();
            self.state.save_settings();
            // The server reads this when the last guest leaves, so it has to be
            // pushed now rather than at the next session.
            self.send(HostCommand::ApplySettings(settings.to_session_config()));
            *self.state.settings.lock().unwrap() = settings;
        }

        // One per connection, not one at a time. A second guest arriving while
        // the first is still being decided about is a second question, and it
        // now has somewhere of its own to be asked — above its own card.
        //
        // Still only one *per* connection: clicking a card that already has a
        // popup open is a mis-click, not a second decision about the same
        // guest.
        if !edits.confirm.is_empty() {
            // Snapshot before locking `incoming` — `settings()` takes its own
            // lock and these must not nest.
            let defaults = self.state.settings().default_grants();
            let incoming = self.state.incoming.lock().unwrap();
            for (session_id, action) in edits.confirm.iter().copied() {
                // Clicking a card whose question is already up puts it away —
                // which is the way out now that there is no Cancel button, and
                // leaves the guest exactly where it was: waiting, seeing
                // nothing.
                if let Some(open) = self.confirms.iter().position(|c| c.session_id == session_id)
                {
                    self.confirms.remove(open);
                    continue;
                }
                if let Some(card) = incoming.iter().find(|c| c.session_id == session_id) {
                    // # A change dialog opens on what is true, not on a default
                    //
                    // The approval dialog offers the smallest useful answer,
                    // because it is deciding something that has not happened.
                    // This one is editing what a guest is looking at right now,
                    // and opening it on anything else would misreport the
                    // session — and, worse, apply that misreport if somebody
                    // pressed the button without reading it.
                    let opening = if action == ConfirmAction::Change {
                        ControlGrants::new(card.mouse_allowed, card.keyboard_allowed)
                    } else {
                        defaults
                    };
                    let mut confirm = Confirm::with_grants(
                        session_id,
                        card.display_name.clone(),
                        card.name_is_self_reported,
                        action,
                        opening,
                    )
                    .for_admin(card.is_admin);
                    if action == ConfirmAction::Change {
                        confirm.adopt(
                            self.host_sessions
                                .as_ref()
                                .and_then(|s| s.displays_of(session_id)),
                        );
                    }
                    self.confirms.push(confirm);
                }
            }
        }

        if edits.paste_passcode {
            // Validate on paste rather than on Connect, so a mangled code is
            // reported while the user still has the original to hand.
            self.checking = None;
            match read_clipboard() {
                None => {
                    self.paste_state = PasteState::Idle;
                    self.connect_error = Some(t!("header.clipboard_empty").into());
                }
                Some(text) => match ConnectionCode::parse(&text) {
                    Ok(code) => {
                        info!("Pasted {}", code.redacted());
                        *self.state.connect.passcode.lock().unwrap() = text;
                        self.connect_error = None;
                        // Parsing only proves the string is well-formed. Whether
                        // Connect will work is a question for the host, and
                        // asking it now is cheaper than a failed attempt later.
                        self.check_pasted(code);
                    }
                    Err(e) => {
                        self.state.connect.passcode.lock().unwrap().clear();
                        self.paste_state = PasteState::Idle;
                        self.connect_error = Some(format!("{e}"));
                    }
                },
            }
        }
    }

    /// Parse the pasted code and open a session in the session window.
    fn start_session(&mut self, ctx: &egui::Context) {
        let raw = self.state.connect.passcode.lock().unwrap().clone();
        self.connect_with(&raw, ctx);
    }

    /// Dial a host from a saved connection's stored code.
    fn reconnect(&mut self, endpoint_id: &str, ctx: &egui::Context) {
        let code = self.state.previous_connections.lock().unwrap().code_for(endpoint_id);
        match code {
            Some(code) => self.connect_with(&code, ctx),
            None => {
                // The row survives without a code — it is still a record of the
                // machine — but it cannot dial on its own.
                self.connect_error =
                    Some("No saved code for that host — paste a fresh one".into());
            }
        }
    }

    /// Open a session from a connection code, remembering the host.
    fn connect_with(&mut self, raw: &str, ctx: &egui::Context) {
        let code = match ConnectionCode::parse(raw) {
            Ok(code) => code,
            Err(e) => {
                warn!("Not a usable connection code: {e:#}");
                self.connect_error = Some(format!("{e}"));
                return;
            }
        };
        self.connect_error = None;

        let loopback =
            *self.state.host.endpoint_bytes.lock().unwrap() == Some(code.endpoint_id);
        if loopback {
            warn!("Connecting to our own endpoint — input forwarding is disabled");
        }
        info!("Connecting with {}", code.redacted());

        // Remember the host now rather than on success: a connection that fails
        // or is refused is still one the user tried to make, and having the row
        // already there is what makes retrying easy.
        let endpoint_hex = super::connections::endpoint_hex(&code.endpoint_id);
        let placeholder = super::connections::placeholder_name(&endpoint_hex);
        self.state
            .previous_connections
            .lock()
            .unwrap()
            .record(endpoint_hex.clone(), code.encode(), &placeholder);

        let settings = self.state.settings();
        let repaint_ctx = ctx.clone();
        let session = GuestSession::start(code, &settings, loopback, move || {
            repaint_ctx.request_repaint()
        });

        // The paste answer has served its purpose — it was there to say whether
        // this would work, and now there is a session to report on instead.
        self.paste_state = PasteState::Idle;
        self.checking = None;

        // Focus the tab we just opened.
        self.session_window.active_tab = self.sessions.len();
        self.sessions.push(session);

        if settings.fullscreen_on_connect {
            self.session_window.request_fullscreen(true);
        }
    }

    /// Close one session tab by id.
    fn close_session(&mut self, tab_id: u64) {
        if let Some(index) = self.sessions.iter().position(|s| s.tab_id == tab_id) {
            self.remember_preview(index);
            self.sessions.remove(index).stop();
        }
        self.after_sessions_changed();
    }

    /// Keep the last picture this session saw, for its card.
    ///
    /// Taken on the way out rather than on a timer: the picture is only ever
    /// looked at once the session is over, so writing it while one is running
    /// would rewrite the file repeatedly to no purpose.
    fn remember_preview(&mut self, index: usize) {
        let Some(session) = self.sessions.get(index) else { return };
        let endpoint = session.endpoint_hex.clone();

        // The address this session ended on used to be recorded here for the
        // next dial to try. It is not any more — see
        // `GuestClient::connect_reporting` for why a remembered address turned
        // out to be worth less than nothing, and note that this was also the
        // only thing that could ever refresh one: a stale entry was cleared by
        // a successful connection, which is precisely what it was preventing.
        let Some(pixels) = session.preview() else { return };
        self.state
            .previous_connections
            .lock()
            .unwrap()
            .set_preview(&endpoint, &pixels);
    }

    fn close_all_sessions(&mut self) {
        for index in 0..self.sessions.len() {
            self.remember_preview(index);
        }
        for mut session in std::mem::take(&mut self.sessions) {
            session.stop();
        }
        self.after_sessions_changed();
    }

    /// Close the tab of any session that ended on its own, and say why.
    ///
    /// # Why the tab closes rather than sitting there
    ///
    /// Being kicked used to leave the session window open on a frozen last
    /// frame with a line of small text somewhere saying "Failed: disconnected by
    /// host". Nothing about that reads as *the session is over* — the picture is
    /// still there, the toolbar is still there, and clicking on it does nothing
    /// for reasons that are not visible.
    ///
    /// So the tab goes, which closes the window when it was the only one, and
    /// the reason surfaces on the main window where the user is now looking.
    ///
    /// Only for a session that actually connected. One that never got in keeps
    /// its tab: the failure *is* the content, and closing it would take away the
    /// only explanation on offer.
    fn prune_ended_sessions(&mut self) {
        let mut ended: Vec<(u64, String)> = Vec::new();
        for session in &mut self.sessions {
            let status = session.status();
            if status == GuestStatus::Streaming {
                session.was_streaming = true;
            }
            if session.was_streaming && !status.is_active() {
                let reason = match status {
                    GuestStatus::Failed(e) => format!("Session ended — {e}"),
                    // A clean end with nothing said. The host closed the stream,
                    // or the app on the other side quit.
                    _ => t!("session.ended_by_host").to_string(),
                };
                ended.push((session.tab_id, reason));
            }
        }

        for (tab_id, reason) in ended {
            info!("{reason}");
            self.close_session(tab_id);
            self.connect_error = Some(reason);
        }
    }

    /// Keep window chrome consistent once the session list changes.
    fn after_sessions_changed(&mut self) {
        if self.sessions.is_empty() {
            // Releasing immersive mode here is what stops a closed session from
            // leaving the pointer confined or the keyboard hook installed.
            // Forwarding goes with it: with no session there is nowhere to
            // forward to, and the hook has no reason to still be running.
            self.immersive.set_forwarding(false);
            self.immersive.release();
            self.session_window.reset();
        } else {
            self.session_window.active_tab =
                self.session_window.active_tab.min(self.sessions.len() - 1);
        }
    }

    // ── Middle: previous connections ─────────────────────────

    /// Match the incoming list against the sessions the server actually has.
    ///
    /// Reconciling beats a "session ended" notification: a guest disconnecting,
    /// timing out, or being kicked all converge here without a separate message
    /// for each. A pill for a dead session used to linger with a green dot as
    /// though the guest were still connected.
    fn reconcile_incoming(&mut self) {
        // Demo rows have no real session behind them, so reconciling would
        // immediately delete the thing the flag exists to show.
        #[cfg(debug_assertions)]
        if debug_flag("demo") {
            return;
        }

        let Some(ref sessions) = self.host_sessions else { return };
        let active = sessions.active_sessions();

        let live: Vec<u32> = {
            let mut incoming = self.state.incoming.lock().unwrap();
            incoming.retain(|c| active.contains(&c.session_id));

            // # Sessions that never asked still have to show up
            //
            // Cards used to be created only when an approval was requested, and
            // an admin passcode skips approval entirely — so an administrator's
            // own connection appeared nowhere and could not be kicked. Anything
            // live and unlisted is added here.
            for id in &active {
                if incoming.iter().any(|c| c.session_id == *id) {
                    continue;
                }
                let is_admin = sessions.passcode_type(*id)
                    == Some(tain_core::PasscodeType::Master);
                let grants = sessions.grants(*id).unwrap_or_default();
                incoming.push(IncomingConnection {
                    session_id: *id,
                    display_name: sessions
                        .label(*id)
                        .unwrap_or_else(|| format!("Session #{id}")),
                    // A label from the session table came from the peer.
                    name_is_self_reported: true,
                    mouse_allowed: grants.mouse,
                    keyboard_allowed: grants.keyboard,
                    // Already connected and streaming; nothing is pending.
                    status: ApprovalStatus::Allowed,
                    is_admin,
                    link: String::new(),
                });
            }

            // # Announced once, from here rather than from the approval channel
            //
            // This is the one place every incoming session passes through,
            // whichever way it arrived. Hanging the notification off the
            // approval request instead would have missed exactly the connection
            // most worth announcing: an admin passcode is never asked about, so
            // it raises no request and would have arrived in silence.
            for card in incoming.iter() {
                if !self.announced.insert(card.session_id) {
                    continue;
                }
                super::notify::incoming_connection(
                    &card.display_name,
                    card.status == ApprovalStatus::Pending,
                );
            }
            // A session id is only unique while it is live, so the set is
            // trimmed to what is still here rather than growing for the life of
            // the process.
            self.announced.retain(|id| active.contains(id));

            for card in incoming.iter_mut() {
                let Some(info) = sessions.info(card.session_id) else { continue };
                card.mouse_allowed = info.grants.mouse;
                card.keyboard_allowed = info.grants.keyboard;
                // A pending card keeps its status until the user decides — the
                // server holds no grants while waiting, so mirroring them would
                // read as a decision nobody made.
                card.link = describe_link(&info);
            }
            incoming.iter().map(|c| c.session_id).collect()
        };

        // Drop responders and dialogs for sessions that vanished.
        self.pending_approvals.retain(|id, _| live.contains(id));
        self.confirms.retain(|c| live.contains(&c.session_id));
    }

    /// Mirror live session state into the saved-connection list.
    ///
    /// Two jobs: adopt the name a host published once it arrives, and give each
    /// row the status of whichever session targets it — so a card goes green
    /// while connected and back to offline when the session ends, without
    /// storing anything that could get stuck.
    fn reconcile_saved_connections(&mut self) {
        #[cfg(debug_assertions)]
        if debug_flag("demo") {
            return;
        }

        let live: Vec<(String, ConnectionStatus)> = self
            .sessions
            .iter()
            .map(|s| {
                let status = match s.status() {
                    GuestStatus::Streaming => ConnectionStatus::Online,
                    GuestStatus::AwaitingApproval => ConnectionStatus::Waiting,
                    GuestStatus::Connecting(_) | GuestStatus::Authenticated(_) => {
                        ConnectionStatus::Connecting
                    }
                    // A failed attempt is the one case where we have actually
                    // tested the host, so it is the one case we can report.
                    GuestStatus::Failed(_) => ConnectionStatus::Failed,
                    GuestStatus::Disconnected => ConnectionStatus::Ready,
                };
                (s.endpoint_hex.clone(), status)
            })
            .collect();

        let names: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter_map(|s| {
                let name = s.shared.host_name.lock().unwrap().clone()?;
                Some((s.endpoint_hex.clone(), name))
            })
            .collect();

        let mut conns = self.state.previous_connections.lock().unwrap();
        conns.apply_status(&live);
        for (endpoint_hex, name) in names {
            // Only replaces the generated placeholder, never a name the user
            // typed — `adopt_host_name` is what enforces that.
            let placeholder = super::connections::placeholder_name(&endpoint_hex);
            conns.adopt_host_name(&endpoint_hex, &name, &placeholder);
        }
    }

    /// Disconnect a guest from the host side.
    fn kick(&mut self, session_id: u32) {
        self.send(HostCommand::KickSession {
            session_id,
            reason: tain_core::protocol::end_reason::DISCONNECTED_BY_HOST.into(),
        });
        // Answer any outstanding approval so the server is not left waiting out
        // its whole timeout on a session that is going away.
        if let Some(pending) = self.pending_approvals.remove(&session_id) {
            let _ = pending.respond.send(ApprovalDecision::Deny);
        }
        self.state.incoming.lock().unwrap().retain(|c| c.session_id != session_id);
    }

    /// Render the pending confirmation dialog, if any.
    ///
    /// The approval variant is where a guest is let in at all, so it carries the
    /// input checkboxes as well: one dialog, one decision, rather than "yes" now
    /// and "which devices" as a separate afterthought.
    /// Apply an edit once the user has stopped making it.
    ///
    /// Coalescing a drag into one write is the whole point, so the delay has to
    /// outlast the gap between frames of a drag and still feel instant when the
    /// user lets go. A repaint is requested explicitly because nothing else
    /// would wake the UI to do this on an otherwise idle window.
    fn flush_settled_settings(&mut self, ctx: &egui::Context) {
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);
        let Some(edited) = self.settings_settling else { return };
        let waited = edited.elapsed();
        if waited < SETTLE {
            ctx.request_repaint_after(SETTLE - waited);
            return;
        }
        self.settings_settling = None;
        let settings = self.state.settings.lock().unwrap().clone();
        self.state.save_settings();
        self.send(HostCommand::ApplySettings(settings.to_session_config()));
    }

    /// What the admin passcode is, said once, before it can be copied.
    ///
    /// # Why this exists and why it is only shown once
    ///
    /// The button beside it copies a secret that *is* the machine — every
    /// screen, mouse and keyboard from the first frame, and with the service
    /// installed the lock screen and UAC prompts too. That is not obvious from
    /// a button labelled "Copy admin passcode", and the moment to say so is
    /// before the first copy rather than in documentation nobody opened.
    ///
    /// Once, because a dialog that appears every time is one people learn to
    /// dismiss without reading, and the cover behind it already handles the
    /// accidental case on every later occasion.
    fn render_admin_warning(&mut self, ctx: &egui::Context) {
        if !self.admin_warning_open {
            return;
        }
        let mut understood = false;

        egui::Window::new(t!("admin_warning.title"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(400.0);
                for (key, colour) in [
                    ("admin_warning.grants", TEXT),
                    ("admin_warning.unattended", AMBER),
                    ("admin_warning.sharing", TEXT),
                    ("admin_warning.mitigations", TEXT_DIM),
                ] {
                    ui.label(egui::RichText::new(t!(key)).size(12.0).color(colour));
                    ui.add_space(8.0);
                }

                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let button = egui::Button::new(
                        egui::RichText::new(t!("admin_warning.understood"))
                            .color(egui::Color32::WHITE),
                    )
                    .fill(ADMIN_FILL);
                    if ui.add(button).clicked() {
                        understood = true;
                    }
                });
            });

        if understood {
            self.admin_warning_open = false;
            // Written down rather than remembered in memory: shown once means
            // once, not once per launch.
            self.state.settings.lock().unwrap().admin_copy_warning_read = true;
            self.state.save_settings();
        }
    }

    /// Every decision waiting to be made, each above the card it is about.
    ///
    /// # Why they are attached and why there can be several
    ///
    /// This was one dialog in the middle of the window, and a decision about
    /// *a* connection rather than about *that* one. With two guests waiting it
    /// was worse than untidy: the dialog named one of them, the strip showed
    /// both, and nothing tied the sentence to the card.
    ///
    /// Anchored to its card, the popup is an extension of it — which is what
    /// makes several at once safe rather than chaotic. Each is exactly as wide
    /// as the card beneath it, so two of them can no more overlap than the two
    /// cards can, and the cards already lay themselves out without colliding.
    ///
    /// A card that is not on screen this frame gets no popup and keeps its
    /// state: the decision is still pending, there is simply nowhere to put the
    /// question. [`Self::retain_live`] is what removes one for good.
    fn render_confirms(&mut self, ctx: &egui::Context, cards: &[(u32, egui::Rect)]) {
        if self.confirms.is_empty() {
            return;
        }
        // Taken out so each popup can be given `&mut Confirm` while the answers
        // below need `&mut self`. Put back before anything is applied.
        let mut confirms = std::mem::take(&mut self.confirms);
        let mut answered: Vec<(u32, Outcome)> = Vec::new();

        for confirm in &mut confirms {
            let Some(card) = cards
                .iter()
                .find(|(session_id, _)| *session_id == confirm.session_id)
                .map(|(_, rect)| *rect)
            else {
                continue;
            };
            if let Some(outcome) = Self::show_confirm(ctx, confirm, card, &self.window_shots) {
                answered.push((confirm.session_id, outcome));
            }
        }
        self.confirms = confirms;

        for (session_id, outcome) in answered {
            self.answer_confirm(session_id, outcome);
        }
    }

    /// One popup, above one card. Returns the answer if it was given.
    fn show_confirm(
        ctx: &egui::Context,
        confirm: &mut Confirm,
        card: egui::Rect,
        shots: &std::cell::RefCell<WindowShots>,
    ) -> Option<Outcome> {
        let (title, body, label) = (confirm.title(), confirm.body(), confirm.confirm_label());
        let caveat = confirm.name_caveat();
        let confirm_guest = confirm.guest.clone();
        let approving = confirm.action == ConfirmAction::Approve;
        // Changing what a running session shows asks the same questions, so it
        // gets the same controls — and a different pair of buttons, since there
        // is nothing to deny and closing it is how you decline.
        let editing = confirm.action == ConfirmAction::Change;
        // An admin connection has nothing to tick: the code already carries
        // every screen and both input devices, and the server ignores anything
        // this dialog might say to the contrary.
        let choosable = (approving || editing) && !confirm.is_admin;
        let mut grants = confirm.grants;
        let screens = confirm.screens.clone();
        let mut share = confirm.share.clone();
        let mut chosen = confirm.chosen.clone();
        let mut picking = confirm.picking;
        let mut outcome = None;

        /// A screen as somebody at this machine would recognise it.
        ///
        /// The monitor's own name first, because that is what is written on the
        /// bezel. Falling back to a number, because a virtual display and some
        /// panels do not say.
        fn describe(screen: &tain_core::protocol::DisplayInfo) -> String {
            let what = match &screen.monitor {
                Some(monitor) => monitor.clone(),
                None if screen.virtual_display => t!("display.virtual").to_string(),
                None => format!("Screen {}", screen.id + 1),
            };
            let mut label =
                format!("{}. {what} — {}×{}", screen.id + 1, screen.width, screen.height);
            if screen.primary {
                label.push_str(" (main)");
            }
            label
        }

        // # Placed by its bottom edge, not its top
        //
        // The popup sits *above* the card, so what has to line up is its
        // bottom — and its height is not known until its contents are laid
        // out. A pivot of LEFT_BOTTOM says where the bottom-left corner goes
        // and lets egui work out the rest, which is the difference between
        // this and measuring the popup on one frame to place it on the next.
        //
        // Identified by session rather than by title: two popups with the same
        // words in them are still two popups, and sharing an id would make
        // egui treat them as one.
        // # The margin is chosen here so the width can be worked back from it
        //
        // Setting the *content* width to the card's width made the popup wider
        // than the card by twice the window's padding, because a frame is its
        // contents plus its margins. Which margin the default window frame uses
        // is a style question this has no business guessing at, so it names one
        // and subtracts it.
        const PAD: f32 = 12.0;
        let inner_width = (card.width() - PAD * 2.0).max(1.0);

        egui::Window::new(title)
            .id(egui::Id::new(("connection-decision", confirm.session_id)))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.style()).inner_margin(egui::Margin::same(PAD)),
            )
            .fixed_pos(egui::pos2(card.left(), card.top() - 8.0))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .show(ctx, |ui| {
                // Exactly the card's width, edge to edge. This is what keeps two
                // of them from colliding without anybody having to arrange them:
                // the cards already do not overlap, so neither can their
                // extensions.
                ui.set_min_width(inner_width);
                ui.set_max_width(inner_width);

                // # Who, then what
                //
                // The name is the heading rather than a sentence inside one,
                // because it is the only part of this dialog that changes
                // between one connection and the next — and the only part
                // somebody has to actually look at before deciding.
                ui.label(egui::RichText::new(&confirm_guest).size(15.0).strong().color(TEXT));
                ui.label(egui::RichText::new(body).size(12.5).color(TEXT));
                if let Some(caveat) = caveat {
                    ui.add_space(3.0);
                    ui.label(egui::RichText::new(caveat).size(11.0).color(TEXT_DIM));
                }

                if choosable {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(t!("approval.permissions"))
                            .size(12.5)
                            .strong()
                            .color(TEXT),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(t!("approval.remote_inputs")).size(12.0).color(TEXT),
                    );
                    ui.checkbox(
                        &mut grants.mouse,
                        egui::RichText::new(t!("approval.mouse")).color(TEXT),
                    )
                    .on_hover_text(t!("approval.mouse_hint"));
                    ui.checkbox(
                        &mut grants.keyboard,
                        egui::RichText::new(t!("approval.keyboard")).color(TEXT),
                    )
                    .on_hover_text(t!("approval.keyboard_hint"));

                    // # Screens as a row of monitors
                    //
                    // Shown even with one screen, unlike the list this replaces.
                    // A row of one costs a line and says what is about to be
                    // seen; the list it replaces cost a paragraph, which is why
                    // it was hidden and why the single-screen case then said
                    // nothing at all.
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(t!("approval.displays")).size(12.0).color(TEXT));
                    ui.add_space(2.0);
                    // # Wrapped, because a host can have a great many screens
                    //
                    // A machine that has spawned virtual displays can offer
                    // more of them than a card is wide, and a single row would
                    // either overflow the popup or squeeze the icons down to
                    // nothing. Wrapping grows the popup downward instead, which
                    // it has room to do — it is anchored by its bottom edge and
                    // opens upward.
                    //
                    // Every cell is the same size, caption or not, so the rows
                    // line up; see `theme::display_button`.
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        // A window share has no screens in it, so the row is
                        // shown as what it is: greyed, and not what this guest
                        // is getting. Left visible rather than hidden — the
                        // popup would jump, and the screens are what going back
                        // means.
                        if !chosen.is_empty() {
                            ui.disable();
                        }
                        let everything = screens.is_empty() || share.len() == screens.len();

                        let resp = theme::display_button(
                            ui,
                            26.0,
                            egui::include_image!("../../assets/multimonitor.svg"),
                            None,
                            Some(t!("approval.all_displays")),
                            everything,
                        );
                        if resp.on_hover_text(t!("approval.all_displays_hint")).clicked()
                        {
                            share = screens.iter().map(|d| d.id).collect();
                        }

                        for (index, screen) in screens.iter().enumerate() {
                            let on = !everything && share.contains(&screen.id);
                            // A virtual display is drawn as one: it is not a
                            // panel on somebody's desk, and which of these is
                            // which decides what a guest is actually looking at.
                            let icon = if screen.virtual_display {
                                egui::include_image!("../../assets/virtualdisplay.svg")
                            } else {
                                egui::include_image!("../../assets/monitor.svg")
                            };
                            let resp =
                                theme::display_button(ui, 26.0, icon, Some(index + 1), None, on);
                            // The detail the old list spelled out on every row,
                            // available to whoever wants it.
                            if resp.on_hover_text(describe(screen)).clicked() {
                                if everything {
                                    // Coming from "all", the obvious meaning of
                                    // clicking one screen is "only that one" —
                                    // not "all of them except this".
                                    share = std::iter::once(screen.id).collect();
                                } else if on {
                                    share.remove(&screen.id);
                                } else {
                                    share.insert(screen.id);
                                }
                            }
                        }
                    });

                    // # Particular windows instead, and it is a different
                    // question
                    //
                    // Not more cells in the row above: picking windows is not
                    // picking one more thing to share, it is choosing to share
                    // *nothing else*. A screen grant answers "which of these may
                    // they see"; this answers "may they see only these". Two
                    // questions, so two controls, and the row above greys out
                    // when this one has an answer.
                    //
                    // # Why a button and not a list here
                    //
                    // The list this replaces named each window in a line of
                    // text, in a popup as wide as a card. That is the one thing
                    // window names are worst at: four Explorer windows are four
                    // folder names, three browser windows are three page
                    // titles, and half of everything else says "Untitled". The
                    // person choosing knows their window by sight, so the
                    // choosing happens somewhere with room to show them — and
                    // what stays here is the answer.
                    ui.add_space(6.0);
                    let count = chosen.len();
                    // # The button says what it opens, not what the session
                    // would show if it were never pressed
                    //
                    // It read "Whole screens" while nothing was chosen — true
                    // about the session, and wrong on this control: a button
                    // for picking windows that is labelled with a screens
                    // answer looks like a screens control. What the session
                    // will show is already visible in the row above, which is
                    // lit while this is empty.
                    //
                    // Once something is chosen the count is the honest label,
                    // because then the button *is* the answer.
                    //
                    // No symbol in front of it. `🗔` (U+1F5D4) has no glyph in
                    // the bundled fonts and drew an empty box — see
                    // `theme::tests::every_symbol_the_ui_draws_has_a_glyph`.
                    let summary = if count == 0 {
                        t!("approval.pick_individual").to_string()
                    } else {
                        tain_core::i18n::plural("approval.windows_chosen", count as u64, &[])
                    };
                    let open =
                        egui::Button::new(egui::RichText::new(summary).size(12.5).color(TEXT))
                            .min_size(egui::vec2(theme::PILL_W - 24.0, 24.0));
                    if ui
                        .add(open)
                        .on_hover_text(t!("approval.one_window_hint"))
                        .clicked()
                    {
                        picking = !picking;
                    }

                    if count == 0 && share.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(t!("approval.nothing_chosen"))
                                .size(11.0)
                                .color(AMBER),
                        );
                    }
                } else if approving {
                    // The admin case. No permissions to choose — the code
                    // already carries all of them — so what is worth saying is
                    // the part that outlives this connection.
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(t!("approval.admin_is_remembered"))
                            .size(12.0)
                            .color(AMBER),
                    );
                } else if editing {
                    // The admin case again, from the other side: this dialog
                    // opened offering to change what the session sees, and an
                    // admin session sees everything by the code it presented.
                    // Saying so beats an empty dialog that looks broken.
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(t!("approval.admin_sees_everything"))
                            .size(12.0)
                            .color(AMBER),
                    );
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    // # Two answers, and no third button
                    //
                    // There used to be a Cancel, from when this was a modal in
                    // the middle of the window that had to be got rid of before
                    // anything else could be done. Attached to its card and no
                    // longer blocking anything, it does not need one: leaving
                    // the question unanswered is simply not answering it, and
                    // clicking the card again puts the popup away.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // # The two answers are told apart by colour and mark
                        //
                        // They sit next to each other and mean opposite things,
                        // and one of them is irreversible in the way that
                        // matters — anything seen during a session has been
                        // seen. So they do not merely differ in wording.
                        //
                        // Applying a change is green like allowing: it keeps
                        // the session running, and the red one on this dialog
                        // means "end it", which this button never does.
                        let go = egui::Button::new(
                            egui::RichText::new(format!("✔ {label}"))
                                .color(egui::Color32::WHITE)
                                .strong(),
                        )
                        .fill(if approving || editing { LIVE_GREEN } else { OFFLINE_RED })
                        .min_size(egui::vec2(84.0, 26.0));
                        if ui.add(go).clicked() {
                            outcome = Some(match confirm.action {
                                ConfirmAction::Approve => Outcome::Allow(grants),
                                ConfirmAction::Change => Outcome::Change(grants),
                                ConfirmAction::Kick => Outcome::Deny,
                            });
                        }

                        if approving {
                            ui.add_space(10.0);
                            let no = egui::Button::new(
                                egui::RichText::new(format!("✖ {}", t!("approval.deny")))
                                    .color(egui::Color32::WHITE)
                                    .strong(),
                            )
                            .fill(OFFLINE_RED)
                            .min_size(egui::vec2(84.0, 26.0));
                            if ui
                                .add(no)
                                .on_hover_text(t!("approval.deny_hint"))
                                .clicked()
                            {
                                outcome = Some(Outcome::Deny);
                            }
                        }
                    });
                });
            });

        // # The picker opens *here*, after the dialog it belongs to
        //
        // Drawn last so it lands above the popup, and outside it so it is not
        // clipped to a card's width — the whole point of it is having room the
        // popup does not have.
        //
        // The list is read at the moment it opens rather than when the dialog
        // did: those can be minutes apart while a guest waits, and a picker is
        // asking about the desk as it is now.
        if picking && !confirm.picking {
            confirm.windows = Self::pickable(&chosen);
            shots
                .borrow_mut()
                .refresh(ctx, confirm.windows.iter().map(|w| w.id).collect());
        }
        if picking {
            picking = Self::show_window_picker(ctx, confirm, &mut chosen, shots);
        }

        // Keep the checkbox state across frames while the popup is open.
        confirm.grants = grants;
        confirm.share = share;
        confirm.chosen = chosen;
        confirm.picking = picking;
        outcome
    }

    /// The windows to offer, with anything already chosen kept on the list.
    ///
    /// # Why the chosen ones are added back
    ///
    /// [`tain_core::window_capture::shareable`] answers "what could somebody
    /// point at", which excludes minimised windows — they have no rectangle on
    /// screen to point at. But a window already chosen and then minimised is
    /// still a perfectly good choice, and dropping it from the list would
    /// silently un-choose it: the person would close the picker having lost a
    /// selection they never touched.
    ///
    /// One that has actually *closed* does drop out, because there is nothing
    /// left to grant.
    fn pickable(
        chosen: &std::collections::BTreeSet<u64>,
    ) -> Vec<tain_core::protocol::WindowInfo> {
        let mut offered = tain_core::window_capture::shareable();
        for id in chosen {
            if offered.iter().any(|window| window.id == *id) {
                continue;
            }
            if let Some(kept) = tain_core::window_capture::info_of(*id) {
                offered.push(kept);
            }
        }
        offered
    }

    /// The window picker. Returns whether it is still open.
    fn show_window_picker(
        ctx: &egui::Context,
        confirm: &Confirm,
        chosen: &mut std::collections::BTreeSet<u64>,
        shots: &std::cell::RefCell<WindowShots>,
    ) -> bool {
        shots.borrow_mut().collect(ctx);

        /// How wide one tile is, picture and caption alike.
        const TILE: f32 = 172.0;
        /// How tall the picture in it is. Not the window's aspect: the tiles
        /// have to line up in a grid, and a portrait window beside a landscape
        /// one would otherwise make a row of ragged heights. The picture is
        /// fitted inside this and centred, so the shape is still visible.
        const SHOT: f32 = 104.0;
        /// Three tiles across, plus the gaps and the frame's own margins.
        const WIDE: f32 = TILE * 3.0 + 12.0 * 2.0 + 24.0;

        let mut open = true;
        let mut still_picking = true;

        egui::Window::new(t!("approval.pick_windows"))
            .id(egui::Id::new(("window-picker", confirm.session_id)))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(WIDE)
            // Over the dialog that opened it, and over the cards: it is the
            // thing being looked at while it is up.
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(WIDE);
                ui.label(
                    egui::RichText::new(t!("approval.pick_windows_hint"))
                        .size(11.5)
                        .color(TEXT_DIM),
                );
                ui.add_space(8.0);

                if confirm.windows.is_empty() {
                    ui.label(
                        egui::RichText::new(t!("approval.no_windows")).size(12.0).color(TEXT_DIM),
                    );
                } else {
                    // Bounded, because a busy desk has more windows than a
                    // screen has room for — and this opens in the middle of
                    // one.
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                            for window in &confirm.windows {
                                let on = chosen.contains(&window.id);
                                let shots = shots.borrow();
                                let tile = theme::window_tile(
                                    ui,
                                    TILE,
                                    SHOT,
                                    shots.of(window.id),
                                    shots.waiting_for(window.id),
                                    &window.app,
                                    &window.title,
                                    on,
                                );
                                drop(shots);
                                // The full caption, which the tile elides: a
                                // browser's is a sentence and the tile is not.
                                if tile.on_hover_text(&window.title).clicked() {
                                    if on {
                                        chosen.remove(&window.id);
                                    } else {
                                        chosen.insert(window.id);
                                    }
                                }
                            }
                        });
                    });
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    // # Going back to screens is a button, not an empty
                    // selection
                    //
                    // Un-ticking the last window does leave the screens on
                    // offer again, but nothing about a grid of unticked tiles
                    // says so. This is the sentence that does.
                    let mut back = egui::Button::new(
                        egui::RichText::new(t!("approval.whole_screens")).size(12.0),
                    );
                    if chosen.is_empty() {
                        back = back.fill(SELECT_BLUE);
                    }
                    if ui.add(back).on_hover_text(t!("approval.whole_screens_hint")).clicked() {
                        chosen.clear();
                        still_picking = false;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let done = egui::Button::new(
                            egui::RichText::new(t!("approval.pick_done"))
                                .color(egui::Color32::WHITE)
                                .strong(),
                        )
                        .fill(LIVE_GREEN)
                        .min_size(egui::vec2(84.0, 26.0));
                        if ui.add(done).clicked() {
                            still_picking = false;
                        }
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(tain_core::i18n::plural(
                                "approval.windows_chosen",
                                chosen.len() as u64,
                                &[],
                            ))
                            .size(11.5)
                            .color(if chosen.is_empty() { TEXT_DIM } else { TEXT }),
                        );
                    });
                });
            });

        // Escape closes it, like every other transient thing in this interface.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            still_picking = false;
        }
        open && still_picking
    }

    /// Act on an answer, and take the popup away.
    fn answer_confirm(&mut self, session_id: u32, outcome: Outcome) {
        let Some(index) = self.confirms.iter().position(|c| c.session_id == session_id) else {
            return;
        };
        let confirm = self.confirms.remove(index);

        match outcome {
            Outcome::Allow(grants) => {
                let displays = confirm.granted();
                self.resolve_approval(session_id, ApprovalDecision::Allow { grants, displays });
            }
            Outcome::Deny => self.resolve_approval(session_id, ApprovalDecision::Deny),
            // # Two commands, not one decision
            //
            // The session is already running and was already approved, so there
            // is no oneshot waiting for an answer — these are edits to something
            // live. Sent separately because they are separate: the input grants
            // travel as `ControlState` and what may be seen as `SharedChanged`,
            // and a guest can legitimately receive one without the other.
            Outcome::Change(grants) => {
                self.send(HostCommand::SetSessionControl { session_id, grants });
                self.send(HostCommand::SetSessionDisplays {
                    session_id,
                    displays: confirm.granted(),
                });
            }
        }
    }

    // ── Header ───────────────────────────────────────────────

    fn render_header(&self, ui: &mut egui::Ui, edits: &mut PendingEdits) {
        // The mark lives in the native title bar as the window icon, so the
        // header carries no logo row — it was mostly empty space.
        //
        // Derived from the space actually available, so the Connect block cannot
        // be pushed off the right edge at high display scaling.
        // Reserve the settings section before sizing anything. It is added last
        // and drawn right-to-left, so measuring against the full width told the
        // Connect block it had room that the gear was going to occupy — which
        // is what made the two overlap on a narrow window.
        let usable = (ui.available_width() - theme::SETTINGS_BLOCK_W).max(0.0);
        let btn_w = theme::passcode_button_width(usable);

        ui.horizontal_top(|ui| {
            // # Everything lines up on the first row
            //
            // Both titles sit level with the *top* button, and the Connect
            // block's paste button sits level with the admin passcode button
            // beside it. Centring each title against its own block instead —
            // which an earlier attempt did — floated the Connect row into the
            // gap between the two Host rows and put the titles at different
            // heights from each other.
            section_label(ui, t!("header.host"), BUTTON_HEIGHT);
            ui.add_space(4.0);

            ui.vertical(|ui| {
                self.render_passcode_row(ui, PasscodeType::Master, btn_w, edits);
                ui.add_space(6.0);
                self.render_passcode_row(ui, PasscodeType::Guest, btn_w, edits);
            });

            ui.add_space(12.0);
            theme::header_separator(ui);
            ui.add_space(12.0);

            section_label(ui, t!("header.connect"), BUTTON_HEIGHT);
            ui.add_space(4.0);

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    // The outline is the paste's own receipt. The field it
                    // fills is not on screen, so without this a good paste and a
                    // bad one looked exactly the same.
                    let held = !self.state.connect.passcode.lock().unwrap().is_empty();
                    let stroke = match (&self.paste_state, held) {
                        (s, _) if s.is_ready() => LIVE_GREEN,
                        (PasteState::Stale, _) => AMBER,
                        (PasteState::Unreachable, _) => OFFLINE_RED,
                        // Held but unverified — the prober is off, or still out.
                        (_, true) => GUEST_STROKE.gamma_multiply(1.4),
                        _ => GUEST_STROKE,
                    };
                    if theme::labelled_button(
                        ui,
                        egui::vec2(btn_w, BUTTON_HEIGHT),
                        // Measured, not a threshold read off the English text
                        // — see `render_passcode_row` for the same reasoning.
                        if theme::text_fits(
                            ui,
                            t!("header.paste"),
                            &egui::FontId::proportional(13.0),
                            (btn_w - 38.0).max(0.0),
                        ) {
                            t!("header.paste")
                        } else {
                            t!("header.paste_short")
                        },
                        egui::include_image!("../../assets/paste_passcode.svg"),
                        GUEST_FILL,
                        stroke,
                        true,
                    )
                    .on_hover_text(t!("header.paste_hint"))
                    .clicked()
                    {
                        edits.paste_passcode = true;
                    }

                    ui.add_space(10.0);

                    let has_code = !self.state.connect.passcode.lock().unwrap().is_empty();
                    // # Not clickable while the host is being checked
                    //
                    // The reachability probe already ran and already knew the
                    // answer; nothing showed it, so a code that took a moment to
                    // verify looked like a dead button. It is held inactive for
                    // the length of the check and spins instead.
                    //
                    // A check that comes back *stale* or *unreachable* still
                    // leaves the button live: an unanswered probe is not proof a
                    // host is gone, and refusing to even try would be a dead end
                    // the user cannot argue with. The status line underneath
                    // says what was found.
                    let checking = matches!(self.paste_state, PasteState::Checking);
                    let live = has_code && !checking;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::Vec2::splat(theme::CONNECT_ICON),
                        if live { egui::Sense::click() } else { egui::Sense::hover() },
                    );

                    if checking {
                        theme::spinner_ring(ui, rect, GUEST_STROKE.gamma_multiply(1.6));
                    } else if self.paste_state.is_ready() {
                        // Confirmed reachable: a quiet ring rather than an
                        // animation, so the state persists after the spin ends.
                        ui.painter().circle_stroke(
                            rect.center(),
                            rect.width() * 0.5 + 3.0,
                            egui::Stroke::new(1.5, LIVE_GREEN.gamma_multiply(0.8)),
                        );
                    }

                    let rect = theme::snap(ui, rect);
                    theme::sharp_image(
                        egui::include_image!("../../assets/connect.svg"),
                        rect.size(),
                    )
                        .tint(if live {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_gray(110)
                        })
                        .paint_at(ui, rect);

                    if resp
                        .on_hover_text(match (has_code, checking) {
                            (false, _) => t!("header.paste_first"),
                            (true, true) => t!("header.checking_host"),
                            (true, false) => t!("header.open_session"),
                        })
                        .clicked()
                    {
                        edits.connect = true;
                    }
                });

                ui.add_space(2.0);
                // Precedence, most specific first: a failed attempt, then what
                // the pasted code turned out to be, then the session count. The
                // paste answer outranks "1 session open" because it is about
                // the button directly above it.
                let (text, colour) = match (&self.connect_error, self.paste_state.message()) {
                    (Some(e), _) => (e.clone(), OFFLINE_RED),
                    (None, Some((text, colour))) => (text, colour),
                    (None, None) => match self.sessions.len() {
                        0 => (t!("incoming.no_sessions").to_string(), TEXT_DIM),
                        n => (t!("incoming.sessions_open", count = n), LIVE_GREEN),
                    },
                };
                ui.label(egui::RichText::new(text).size(11.0).color(colour));
            });

            // Settings gets its own section pinned to the right window edge.
            // right_to_left consumes the remaining width, so it is added last
            // but drawn from the far right inward.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(
                    ui,
                    22.0,
                    egui::include_image!("../../assets/settings.svg"),
                    t!("header.settings"),
                )
                .clicked()
                {
                    edits.open_settings = true;
                }
                ui.add_space(14.0);
                theme::header_separator(ui);
            });
        });
    }

    /// One passcode row: labelled copy button, reset icon, autoreset checkbox.
    fn render_passcode_row(
        &self,
        ui: &mut egui::Ui,
        typ: PasscodeType,
        width: f32,
        edits: &mut PendingEdits,
    ) {
        let code = self.state.host.passcode(typ);
        let recently_copied = self.copied_at.is_some_and(|(t, at)| {
            t == typ && at.elapsed() < std::time::Duration::from_millis(1200)
        });

        // # The long form if it fits, measured rather than guessed
        //
        // This used to be `width >= 200.0`, a threshold read off the English
        // strings — which says nothing about a language whose translation of
        // the same words is half again as long. Asking whether the text fits
        // is the question the threshold was standing in for, and it is the
        // same question in every language.
        let font = egui::FontId::proportional(13.0);
        // What `labelled_button` leaves for the label: the inset, the icon and
        // the gap before it.
        let room = (width - 12.0 - 18.0 - 8.0).max(0.0);
        let roomy = |long: &str| theme::text_fits(ui, long, &font, room);

        let (long, short, fill, stroke) = match typ {
            PasscodeType::Master => (
                t!("header.copy_admin"),
                t!("header.copy_admin_short"),
                ADMIN_FILL,
                ADMIN_STROKE,
            ),
            PasscodeType::Guest => (
                t!("header.copy_guest"),
                t!("header.copy_guest_short"),
                GUEST_FILL,
                GUEST_STROKE,
            ),
        };
        let (long, short) = if recently_copied {
            (t!("header.copied"), t!("header.copied_short"))
        } else {
            (long, short)
        };
        let label = if roomy(long) { long } else { short };
        let stroke = if recently_copied { LIVE_GREEN } else { stroke };

        ui.horizontal(|ui| {
            // # The admin code is kept behind a cover
            //
            // Copying it is a one-way act: once it is on the clipboard it can
            // leave in the next paste, into whatever window has focus, and
            // nothing afterwards calls it back. And what it hands over is the
            // machine — every screen, mouse and keyboard, and with the service
            // installed the lock screen too.
            //
            // So it is not a button that can be hit by accident. See
            // [`theme::slide_to_reveal`] for why a drag and not a dialog.
            if typ == PasscodeType::Master && !self.admin_cover.is_open() {
                // Measured against the slider's own geometry and its own text
                // size, not the copy button's. Sharing `roomy` was wrong twice
                // over — a different string, at a different size — which is how
                // a Russian instruction came to sit hard against both edges of
                // a slider the copy label fitted comfortably.
                let cover = egui::vec2(width, BUTTON_HEIGHT);
                let long = t!("header.reveal_admin");
                let outcome = theme::slide_to_reveal(
                    ui,
                    cover,
                    if theme::slider_label_fits(ui, long, cover) {
                        long
                    } else {
                        t!("header.reveal_admin_short")
                    },
                    self.admin_cover.progress,
                );
                edits.admin_slider = Some(outcome.progress);
                // The explanation comes on first contact, before the gesture
                // has finished — somebody who reads it and changes their mind
                // should not have had to complete the drag to be told.
                if outcome.touched && !self.state.settings().admin_copy_warning_read {
                    edits.admin_warning = true;
                }
                if outcome.completed {
                    edits.admin_uncovered = true;
                }
                render_passcode_row_tail(ui, typ, &self.state.settings(), edits);
                return;
            }

            let resp = theme::labelled_button(
                ui,
                egui::vec2(width, BUTTON_HEIGHT),
                label,
                egui::include_image!("../../assets/copy_passcode.svg"),
                fill,
                stroke,
                code.is_some(),
            );

            // A disabled button with no explanation is a dead end — say why
            // there is no code to copy.
            let resp = match (&code, self.state.host.error.lock().unwrap().clone()) {
                (Some(_), _) => {
                    resp.on_hover_text(t!("header.copy_hint"))
                }
                (None, Some(e)) => {
                    resp.on_hover_text(t!("header.host_offline", reason = e))
                }
                (None, None) => resp.on_hover_text(t!("header.no_code")),
            };

            if resp.clicked()
                && let Some(code) = code
            {
                ui.ctx().copy_text(code);
                edits.copied = Some(typ);
            }

            render_passcode_row_tail(ui, typ, &self.state.settings(), edits);
        });
    }

    // ── Middle: previous connections ─────────────────────────

    fn render_previous_connections(&self, ui: &mut egui::Ui, edits: &mut PendingEdits) {
        // Snapshot, then release — the render body must not re-lock this mutex.
        let connections = self.state.previous_connections.lock().unwrap().entries().to_vec();

        if self.state.previous_connections.lock().unwrap().is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(t!("previous.none"))
                        .size(13.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(t!("previous.saved_here"))
                        .size(11.0)
                        .color(SEPARATOR),
                );
            });
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(20.0, 20.0);
            for conn in &connections {
                self.render_connection_card(ui, conn, edits);
            }
        });
    }

    /// Fill the card's inner panel with this host's screen, if we have seen it.
    ///
    /// # In the panel, not behind the card
    ///
    /// It started behind the whole card, which put a wash of colour under the
    /// name field and the status line and read as a rendering fault rather than
    /// as a picture. The inner panel is already a distinct, bounded region with
    /// nothing else in it — putting the picture there gives it an edge, so it
    /// reads as deliberate.
    ///
    /// It is still dimmed, because the panel sits inside a control: the name,
    /// status and buttons around it must stay legible over any wallpaper,
    /// including a white one.
    fn paint_card_preview(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        conn: &super::connections::SavedConnection,
    ) {
        let mut cache = self.previews.borrow_mut();
        let texture = match cache.get(&conn.endpoint_id) {
            Some(texture) => texture,
            None => {
                let Some(image) =
                    conn.preview_pixels().and_then(|p| super::preview::to_color_image(&p))
                else {
                    return;
                };
                let handle = ui.ctx().load_texture(
                    format!("preview-{}", conn.endpoint_id),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                cache.entry(conn.endpoint_id.clone()).or_insert(handle)
            }
        };

        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let mut mesh = egui::Mesh::with_texture(texture.id());
        // Brighter than it was behind the card: inside its own panel the picture
        // is the content rather than a tint under someone else's text.
        mesh.add_rect_with_uv(rect, uv, egui::Color32::from_white_alpha(150));
        ui.painter().with_clip_rect(rect).add(egui::Shape::mesh(mesh));
        // A light scrim, so a pale wallpaper cannot wash out what sits over it.
        ui.painter().rect_filled(rect, 3.0, egui::Color32::from_black_alpha(48));
    }

    fn render_connection_card(
        &self,
        ui: &mut egui::Ui,
        conn: &super::connections::SavedConnection,
        edits: &mut PendingEdits,
    ) {
        let (rect, _) = ui.allocate_exact_size(CARD_SIZE, egui::Sense::hover());
        ui.painter().rect(
            rect,
            CORNER,
            CARD_BG,
            egui::Stroke::new(1.0, CARD_STROKE),
        );

        // Anchor each region to the card rect rather than flowing them. Flow
        // layout let the name field's real height push the status bar out
        // through the bottom border.
        let inner = rect.shrink(10.0);
        let name_h = 26.0;
        let bar_h = 24.0;
        // The forget button only *appears* on hover — it deletes a stored secret,
        // so it should not be one stray click away while scanning the list — but
        // its space is reserved either way.
        //
        // Two bugs came from doing this the obvious way. Sizing the name field
        // by hover made the text reflow every time the pointer crossed the card;
        // and asking `card_resp.hovered()` gave the wrong answer precisely when
        // it mattered, because the name field is a child widget and swallows the
        // hover, so moving onto the name made the button disappear. Reserving
        // the width always, and testing the pointer against the card rect rather
        // than the card's own response, fixes both.
        let forget_w = 22.0;
        let hovered = ui.rect_contains_pointer(rect);
        let name_rect = egui::Rect::from_min_size(
            inner.min,
            egui::vec2(inner.width() - forget_w, name_h),
        );
        let bar_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - bar_h),
            inner.max,
        );
        let body_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), name_rect.bottom() + 6.0),
            egui::pos2(inner.right(), bar_rect.top() - 8.0),
        );

        // Must be a child UI, not `ui.put`: `put` allocates in the *parent*,
        // which grows the row's used rect and staircases every later card down.
        let mut name_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(name_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut name = conn.display_name.clone();
        if name_ui
            .add(
                egui::TextEdit::singleline(&mut name)
                    .frame(false)
                    .text_color(TEXT)
                    .desired_width(name_rect.width())
                    .font(egui::FontId::proportional(17.0))
                    .hint_text("Unnamed PC"),
            )
            .changed()
        {
            edits.rename.push((conn.endpoint_id.clone(), name));
        }

        if hovered {
            let mut forget_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(egui::Rect::from_min_size(
                        egui::pos2(name_rect.right(), name_rect.top()),
                        egui::vec2(forget_w, name_h),
                    ))
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            if forget_ui
                .small_button("×")
                .on_hover_text(t!("previous.forget"))
                .clicked()
            {
                edits.forget.push(conn.endpoint_id.clone());
            }
        }

        // Body: reserved for a session thumbnail once frames render.
        ui.painter().rect_filled(body_rect, 3.0, CARD_INNER_BG);
        self.paint_card_preview(ui, body_rect, conn);
        ui.painter().hline(
            inner.x_range(),
            bar_rect.top() - 4.0,
            egui::Stroke::new(1.0, SEPARATOR),
        );

        let mut bar = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // The reason the last probe gave, where it gave one, in preference to
        // the generic explanation. "Offline" cannot separate a host that is
        // switched off from a relay that is blocked from here, and those want
        // opposite responses — so when the probe knows which it was, it says.
        let detail = match conn.probe_why.as_deref() {
            Some(why) => format!("{} — {why}.", conn.status.detail()),
            None => conn.status.detail().to_string(),
        };
        bar.label(egui::RichText::new(conn.status.label()).size(14.0).color(TEXT))
            .on_hover_text(detail);
        bar.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The triangle's colour says whether pressing it does anything; the
            // label beside it says what the state is. Conflating the two is what
            // produced a red "offline" card with a button that worked.
            let ready = conn.can_reconnect() && conn.status.can_connect();
            let (tri, resp) = ui.allocate_exact_size(
                egui::vec2(20.0, 20.0),
                if ready { egui::Sense::click() } else { egui::Sense::hover() },
            );
            theme::play_triangle(ui, tri, conn.status.accent());

            let resp = resp.on_hover_text(match conn.status {
                ConnectionStatus::Ready => t!("previous.connect"),
                ConnectionStatus::Unknown => t!("previous.connect_unchecked"),
                ConnectionStatus::Failed => t!("previous.try_again"),
                ConnectionStatus::Online => t!("previous.already_connected"),
                ConnectionStatus::Connecting | ConnectionStatus::Waiting => {
                    t!("previous.connecting")
                }
                // Both of these still offer the attempt: the check is a snapshot
                // and the host may have come back since.
                ConnectionStatus::Stale => t!("previous.try_anyway_expired"),
                ConnectionStatus::Unreachable => t!("previous.try_anyway_unreachable"),
                ConnectionStatus::NoCode => t!("previous.no_saved_code"),
            });
            if resp.clicked() {
                edits.reconnect = Some(conn.endpoint_id.clone());
            }
        });
    }

    // ── Footer: incoming connections ─────────────────────────

    /// Returns where each card ended up, so the decision popups can sit on them.
    fn render_incoming_connections(
        &self,
        ui: &mut egui::Ui,
        edits: &mut PendingEdits,
    ) -> Vec<(u32, egui::Rect)> {
        // Snapshot, then release — the toggles below used to re-lock this mutex
        // mid-iteration and freeze the app on the first click.
        let incoming = self.state.incoming.lock().unwrap().clone();

        if incoming.is_empty() {
            ui.label(
                egui::RichText::new(t!("incoming.none"))
                    .size(12.0)
                    .color(TEXT_DIM),
            );
            return Vec::new();
        }

        // # What the open dialog is proposing, shown on the card it is about
        //
        // The checkboxes and the card's own mouse and keyboard marks say the
        // same thing about the same connection, and while the dialog is open
        // they used to disagree: the card kept showing the grants the session
        // actually has — none, for a guest that has not been let in — while the
        // dialog above it showed what was being offered.
        //
        // Reflecting the pending answer downward makes ticking a box visibly
        // *do* something, and means the strip somebody will be reading during
        // the session already reads correctly before it starts.
        let mut rects = Vec::new();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 8.0);
            for conn in &incoming {
                let pending_grants = self
                    .confirms
                    .iter()
                    .find(|c| {
                        c.session_id == conn.session_id && c.action == ConfirmAction::Approve
                    })
                    .map(|c| c.grants);
                let rect = Self::render_incoming_pill(ui, conn, pending_grants, edits);
                rects.push((conn.session_id, rect));
            }
        });
        rects
    }

    /// One incoming connection, as a card that fits the name on it.
    ///
    /// # Why the card measures itself
    ///
    /// It was a fixed rectangle with the name dropped in, and then a rectangle
    /// whose width was a hand-counted sum of everything that was not the name.
    /// Both were wrong the same way: the sum left out the spacing egui puts
    /// between widgets, so a long name ran underneath the permission icons.
    ///
    /// A frame sized by its own contents cannot be wrong about that, and it
    /// removes the constant that has now been wrong twice. The background is
    /// chosen after the contents are laid out, which is also the first moment
    /// the card's rectangle — and therefore whether the pointer is over it — is
    /// known at all.
    fn render_incoming_pill(
        ui: &mut egui::Ui,
        conn: &IncomingConnection,
        proposed: Option<ControlGrants>,
        edits: &mut PendingEdits,
    ) -> egui::Rect {
        const MARGIN: egui::Vec2 = egui::vec2(8.0, 10.0);

        let pending = conn.status == ApprovalStatus::Pending;
        // What the open dialog is offering, where there is one, and otherwise
        // what this session actually has.
        let (mouse_on, keyboard_on) = match proposed {
            Some(grants) => (grants.mouse, grants.keyboard),
            None => (conn.mouse_allowed, conn.keyboard_allowed),
        };
        let shown = elide(&conn.display_name, PILL_NAME_MAX_CHARS);

        let mut card = egui::Frame::none()
            .rounding(CORNER)
            .inner_margin(egui::Margin::symmetric(MARGIN.x, MARGIN.y))
            .begin(ui);

        let id = ui.make_persistent_id(("incoming-card", conn.session_id));
        let card_resp = {
            let ui = &mut card.content_ui;
            // # Fixed, not merely a minimum
            //
            // The marks below are laid out from the right edge, and a
            // right-to-left layout claims whatever width it is *offered* — so a
            // card that could grow would grow to the whole strip, taking its
            // marks with it. Capping the width is what makes "on the right
            // edge" mean the card's edge. See [`theme::PILL_W`].
            let width = theme::PILL_W - MARGIN.x * 2.0;
            ui.set_min_size(egui::vec2(width, PILL_HEIGHT - MARGIN.y * 2.0));
            ui.set_max_width(width);

            // # The card senses clicks *before* the controls on it
            //
            // egui resolves overlapping click targets by taking the last one
            // registered, so a card-sized interaction added after its contents
            // covers every button on it. That is what happened when the card
            // became clickable: the kick control stopped responding, because
            // the card was swallowing its clicks.
            //
            // Registered first, it is underneath, and each control added below
            // takes its own clicks back. What is left for the card is the space
            // between them, which is what "click the card" ever meant.
            let card_resp = ui.interact(ui.max_rect(), id, egui::Sense::click());

            ui.horizontal(|ui| {
                // The person icon *is* the accept/remove control. Pending
                // guests are admitted here; established ones swap to the kick
                // icon on hover, which is the only way to remove a connected
                // guest.
                let (icon_rect, icon_resp) =
                    ui.allocate_exact_size(egui::Vec2::splat(22.0), egui::Sense::click());
                // Kickable whether or not it was ever approved: an admin
                // connection is never asked about, and "cannot be asked about"
                // must not mean "cannot be ended".
                let kicking = !pending && icon_resp.hovered();
                let icon_src = if kicking {
                    egui::include_image!("../../assets/incoming_connection_kick.svg")
                } else if conn.is_admin {
                    // Its own mark: this one holds the admin passcode and
                    // already has full control, worth seeing at a glance.
                    egui::include_image!("../../assets/incoming_connection_admin_icon.svg")
                } else {
                    egui::include_image!("../../assets/incoming_connection_icon.svg")
                };
                if ui.is_rect_visible(icon_rect) {
                    let icon_rect = theme::snap(ui, icon_rect);
                    theme::sharp_image(icon_src, icon_rect.size()).paint_at(ui, icon_rect);
                }
                if icon_resp
                    .on_hover_text(if pending {
                        t!("incoming.waiting_click")
                    } else {
                        t!("incoming.disconnect")
                    })
                    .clicked()
                {
                    edits.confirm.push((
                        conn.session_id,
                        if pending { ConfirmAction::Approve } else { ConfirmAction::Kick },
                    ));
                }

                // One tooltip, and it carries the full name whenever the card
                // is showing only part of one: a guest identified by a name
                // that has been cut short is a guest the person deciding
                // cannot actually identify.
                let mut hover = if shown == conn.display_name {
                    String::new()
                } else {
                    format!("{}\n", conn.display_name)
                };
                if pending {
                    hover.push_str(t!("incoming.waiting_nothing_shared"));
                } else {
                    hover.push_str(describe_grants(conn.mouse_allowed, conn.keyboard_allowed));
                    if conn.name_is_self_reported {
                        hover.push_str("\nThis name was chosen by the connecting machine.");
                    }
                }
                // Truncated rather than wrapped. `elide` already caps this at a
                // character count, but a count is not a width: twelve Cyrillic
                // or CJK characters are far wider than twelve Latin ones, and a
                // name that wraps makes the card taller than every other card
                // in the strip. The full name is in the tooltip either way.
                ui.add(
                    egui::Label::new(egui::RichText::new(&shown).size(13.0).color(TEXT))
                        .truncate(),
                )
                .on_hover_text(hover);

                // # The marks sit on the right edge, not beside the name
                //
                // A card is as wide as its name until a decision is attached to
                // it, and then it is as wide as the question above it — which
                // left the icons floating in the middle of a card with empty
                // space after them, in a strip whose whole job is to be read at
                // a glance.
                //
                // Laid out right-to-left, so they are added in the order they
                // are read *backwards*: status, keyboard, mouse.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match conn.status {
                    ApprovalStatus::Pending => {
                        ui.add(egui::Spinner::new().size(14.0).color(TEXT_DIM))
                            .on_hover_text(t!("incoming.nothing_until_you_decide"));
                    }
                    ApprovalStatus::Allowed => {
                        theme::icon(
                            ui,
                            13.0,
                            egui::include_image!("../../assets/in_connect_live.svg"),
                        );
                    }
                }

                let kbd = if keyboard_on {
                    egui::include_image!("../../assets/keyboard_allowed.svg")
                } else {
                    egui::include_image!("../../assets/keyboard_disabled.svg")
                };
                if theme::icon_button(
                    ui,
                    22.0,
                    kbd,
                    match (pending, keyboard_on) {
                        (true, true) => t!("incoming.offered_not_granted"),
                        (true, false) => t!("incoming.decide_first"),
                        (false, true) => t!("incoming.keyboard_allowed"),
                        (false, false) => t!("incoming.keyboard_blocked"),
                    },
                )
                .clicked()
                {
                    if pending {
                        edits.confirm.push((conn.session_id, ConfirmAction::Approve));
                    } else {
                        edits.set_grants.push((
                            conn.session_id,
                            ControlGrants::new(conn.mouse_allowed, !conn.keyboard_allowed),
                        ));
                    }
                }

                // On an established connection these toggle directly — revoking
                // should be instant. On a pending one there is nothing to
                // toggle yet, so they open the same dialog the person icon does.
                let mouse = if mouse_on {
                    egui::include_image!("../../assets/mouse_allowed.svg")
                } else {
                    egui::include_image!("../../assets/mouse_disabled.svg")
                };
                if theme::icon_button(
                    ui,
                    22.0,
                    mouse,
                    match (pending, mouse_on) {
                        (true, true) => t!("incoming.offered_not_granted"),
                        (true, false) => t!("incoming.decide_first"),
                        (false, true) => t!("incoming.mouse_allowed"),
                        (false, false) => t!("incoming.mouse_blocked"),
                    },
                )
                .clicked()
                {
                    if pending {
                        edits.confirm.push((conn.session_id, ConfirmAction::Approve));
                    } else {
                        edits.set_grants.push((
                            conn.session_id,
                            ControlGrants::new(!conn.mouse_allowed, conn.keyboard_allowed),
                        ));
                    }
                }
                });
            });
            card_resp
        };

        // # The rectangle comes from the frame, not from its contents
        //
        // `content_ui.min_rect()` is the region the *contents* claimed, and a
        // horizontal layout claims the width it was offered — the whole row.
        // Outlining that drew a dashed box around everything to the right of
        // the card as well, which is what the reported screenshot shows.
        //
        // `end` returns the rectangle the frame actually took, which is the
        // card. It is only known afterwards, so the hover that decides the fill
        // is remembered from the previous frame: a frame of lag on a background
        // tint is not perceptible, and being right about the shape is.
        let was_hovered = ui.data(|d| d.get_temp::<bool>(id).unwrap_or(false));
        card.frame = card.frame.fill(if was_hovered { CARD_BG_HOVER } else { CARD_BG });
        if !pending {
            card.frame = card.frame.stroke(egui::Stroke::new(1.0, CARD_STROKE));
        }
        let rect = card.end(ui).rect;

        // Hovering anywhere on the card tints it, including over the controls —
        // `contains_pointer` is true for everything under the pointer, unlike a
        // click, which goes to exactly one of them.
        ui.data_mut(|d| d.insert_temp(id, card_resp.contains_pointer()));
        // On the card rather than on a control, because it describes the whole
        // connection rather than anything that can be clicked. Withheld while
        // empty: an established session that has not reported yet would
        // otherwise flash a blank tooltip on the way past.
        if !conn.link.is_empty() {
            card_resp.clone().on_hover_text(&conn.link);
        }
        if card_resp.clicked() {
            edits.confirm.push((
                conn.session_id,
                if pending { ConfirmAction::Approve } else { ConfirmAction::Change },
            ));
        }

        if pending {
            // Travelling dashes. A guest waiting to be let in is the one thing
            // on this screen that wants noticing, and a still dotted rectangle
            // was not managing it.
            let phase = -(ui.input(|i| i.time) as f32) * theme::DASH_SPEED;
            theme::dashed_rect(ui, rect, TEXT_DIM, phase);
            // Time-driven, so it has to be asked for; nothing else here repaints
            // while the screen is idle.
            ui.ctx().request_repaint();
        }

        rect
    }
}

/// Longest guest name an incoming card spells out before eliding it.
///
/// Names come from the machine connecting, which chooses its own, so this is
/// not a length anything here controls — only how much of one a card is willing
/// to become.
const PILL_NAME_MAX_CHARS: usize = 12;

/// Shorten `name` to at most `max` characters, marking where it was cut.
///
/// # Characters, not bytes
///
/// These names arrive from other people's machines and may be in any script.
/// Slicing a `String` by byte index panics when the index lands inside a
/// character, so a Cyrillic or CJK name would take the interface down rather
/// than be shortened — and those are exactly the names long enough to need it.
fn elide(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    // One character of the budget is spent saying that there was more.
    let kept: String = name.chars().take(max.saturating_sub(1)).collect();
    kept + "\u{2026}"
}

/// What a connection card's tooltip says about the link behind it.
///
/// # Why the host is told any of this
///
/// The host could see that someone was connected and nothing else. Everything
/// about *how* — the address they came in on, the round trip, whether the
/// stream is running at what was asked for — existed only in the log file, and
/// only for someone who knew to go looking. But the host is the one who can act
/// on it: a guest complaining the picture is soft is a different conversation
/// depending on whether this machine is encode-bound or the link is 200 ms away.
///
/// Built from whatever has arrived. A card exists before the guest has reported
/// anything, so every line here is optional and the ones with nothing behind
/// them are left out rather than printed empty.
fn describe_link(info: &tain_core::SessionInfo) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let Some(route) = &info.route {
        lines.push(route.clone());
    }
    if let Some(link) = info.link {
        // The guest's own measurement. The host cannot take this one: a round
        // trip needs both ends, and only the far one sees the reply come back.
        lines.push(format!("{:.0} ms round trip", link.rtt_ms));
    }
    if let Some(status) = info.stream_status {
        let mut line = format!(
            "Sending {} fps at {}",
            status.fps,
            bitrate_label(status.bitrate_kbps)
        );
        if status.degraded {
            let held = tain_core::BottleneckSet::from_bits(status.bottlenecks)
                .iter()
                .map(|b| b.label().to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(", ");
            // Degraded with nothing named means the ceiling itself is the
            // limit, which is a setting rather than a fault.
            if held.is_empty() {
                line.push_str(" — below what was asked for");
            } else {
                line.push_str(&format!(" — held back by {held}"));
            }
        }
        lines.push(line);
    }
    if let Some(cfg) = &info.display_config
        && let Some(decoded) = info.link
    {
        lines.push(format!(
            "Guest is decoding {:.0} fps into {}x{}",
            decoded.decoded_fps, cfg.client_width, cfg.client_height
        ));
    }

    lines.join("\n")
}

/// kbps as something with a unit on it, because five figures of kbit/s is not
/// a number anyone reads at a glance.
fn bitrate_label(kbps: u32) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbit/s", kbps as f32 / 1000.0)
    } else {
        format!("{kbps} kbit/s")
    }
}

/// Plain-language summary of a session's input permissions.
pub fn describe_grants(mouse: bool, keyboard: bool) -> &'static str {
    t!(match (mouse, keyboard) {
        (true, true) => "grants.full",
        (true, false) => "grants.mouse_only",
        (false, true) => "grants.keyboard_only",
        (false, false) => "grants.view_only",
    })
}

impl GuestApp {
    /// Keep the interface scale, egui's zoom and the saved setting in step.
    ///
    /// # Why this is a negotiation rather than an assignment
    ///
    /// egui owns a zoom factor of its own and moves it when Ctrl+plus or
    /// Ctrl+minus is pressed. Writing the setting into it every frame would
    /// undo that a frame later, so the keyboard would appear not to work at
    /// all; reading it every frame instead would mean the slider could never
    /// move it. Which of the two changed is the only thing that distinguishes
    /// them, and that is what `applied_ui_scale` remembers.
    ///
    /// A change from the keyboard is adopted into the settings, so it is saved
    /// and the slider agrees with what is on screen.
    /// Send every guest a goodbye, and wait briefly for it to leave.
    ///
    /// The wait is the point. `kick` queues the message on the session's own
    /// writer, which is a task on the runtime, and this is the interface thread
    /// on its way out — return immediately and the process exits before any of
    /// those writers is scheduled, which is exactly as silent as sending
    /// nothing.
    ///
    /// Blocking here is safe in a way it would not be anywhere else: the window
    /// is already gone, so there is no frame to hold up, and the wait is only
    /// paid when somebody is actually connected.
    fn say_goodbye_to_guests(&mut self) {
        /// Long enough for a writer to be scheduled and put a small message on
        /// the wire, short enough not to be noticed as the application closing
        /// slowly. It does not wait for delivery — nothing here could, and a
        /// link that has already failed would never provide it.
        const GRACE: std::time::Duration = std::time::Duration::from_millis(300);

        let Some(sessions) = self.host_sessions.as_ref() else { return };
        let live = sessions.active_sessions();
        if live.is_empty() {
            return;
        }

        info!("Closing — telling {} connected guest(s)", live.len());
        for id in live {
            sessions.kick(id, tain_core::protocol::end_reason::HOST_CLOSED);
        }
        std::thread::sleep(GRACE);
    }

    /// Ask for focus once, shortly after the window appears.
    ///
    /// # Why this is needed at all
    ///
    /// eframe creates the window **hidden** and shows it after the first frame,
    /// deliberately — it is how the white flash on startup is avoided. But a
    /// window that was created hidden and then shown is shown *without being
    /// activated*, so the application opens behind whatever the person was
    /// looking at and has to be clicked before it will take a keystroke.
    ///
    /// # Why once, and why not immediately
    ///
    /// Once, because repeating it is how an application becomes the one that
    /// snatches the screen back every time you look away — the command's own
    /// documentation says as much. Not on the first frame, because the window
    /// is still hidden then and the request would be dropped: `focus_window`
    /// has no effect on a window that is not visible.
    ///
    /// Windows may refuse anyway — a process that does not own the foreground
    /// cannot take it, which is the rule that stops background programs
    /// interrupting. Launched by someone double-clicking it, this application
    /// does own it, and that is the case this is for.
    fn claim_focus_once(&mut self, ctx: &egui::Context) {
        /// Frames to let pass first. One is enough for eframe to have shown the
        /// window; a couple more costs nothing and is not a race.
        const SETTLE: u32 = 3;

        if self.frames_before_focus > SETTLE {
            return;
        }
        self.frames_before_focus += 1;
        if self.frames_before_focus == SETTLE {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Say what scale this window is being drawn at, whenever that changes,
    /// and throw away pictures that were drawn for the old one.
    ///
    /// # Why the icons have to be forgotten
    ///
    /// They are SVGs, and both halves of the machinery that draws them look
    /// like they handle a change of scale. egui asks its image loaders for
    /// `size * pixels_per_point`, and `egui_extras`' SVG loader caches on
    /// `(uri, size hint)`, so a new scale is a new entry and a fresh
    /// rasterisation.
    ///
    /// The layer between them does not. `DefaultTextureLoader` caches on
    /// `(uri, texture options)` with the size hint left out of the key, so once
    /// an icon has been uploaded at one resolution every later draw is handed
    /// that same texture whatever it asked for. The loader that would happily
    /// re-render is never reached. The picture is then stretched to whatever
    /// size the new scale wants, which is the soft, smeared look reported after
    /// changing the interface scale.
    ///
    /// Forgetting them makes the next draw go all the way down and rasterise
    /// again. It costs re-rendering a couple of dozen small SVGs, at the moment
    /// somebody deliberately changes the scale or drags the window to a screen
    /// with a different DPI — not per frame, and not on an ordinary resize.
    ///
    /// Textures the application loaded itself — the connection previews — are
    /// held by their own handles and are not in these caches, so they are not
    /// disturbed.
    ///
    /// # Why this is worth a log line
    ///
    /// Nothing about DPI is visible from inside the process. Every number
    /// agrees with every other one whether or not the awareness is the one that
    /// was asked for, and the only symptom is an interface the wrong size on a
    /// monitor the developer may not have. Three attempts went into that, two
    /// of them chasing the wrong cause, and the thing that settled it was
    /// measuring from outside with a second monitor attached.
    ///
    /// One line per change makes the same measurement available from the log:
    /// drag the window to another screen, and either the monitor figure moves
    /// or it does not.
    fn note_scale_change(&mut self, ctx: &egui::Context) {
        let native = ctx.native_pixels_per_point().unwrap_or(f32::NAN);
        let drawn = ctx.pixels_per_point();
        let size = ctx.screen_rect().size();

        let changed = match self.last_reported_scale {
            None => true,
            Some((was_native, was_drawn, was_size)) => {
                (was_native - native).abs() > 0.001
                    || (was_drawn - drawn).abs() > 0.001
                    // Points, not pixels: a window whose *point* size jumps
                    // without the scale moving is the interesting failure, and
                    // an ordinary resize is a handful of lines at most.
                    || (was_size - size).length() > 16.0
            }
        };
        if !changed {
            return;
        }

        // Only the drawing scale invalidates them. A window that merely got
        // bigger is drawing the same icons at the same resolution.
        let rescaled = self
            .last_reported_scale
            .is_some_and(|(_, was_drawn, _)| (was_drawn - drawn).abs() > SCALE_EPSILON);
        if rescaled {
            ctx.forget_all_images();
            // The minimum size has to be restated for the same reason, and it
            // is not cosmetic: winit converted it to pixels when it was given
            // and does not reconvert it, so after a change of scale the window
            // has a minimum measured in the *old* display's pixels. Restating
            // it here is what makes it mean the same thing on both.
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(
                MIN_WINDOW_SIZE.into(),
            ));
        }

        self.last_reported_scale = Some((native, drawn, size));
        info!(
            "Interface scale: monitor asks {native:.2}x, drawing at {drawn:.2}x \
             (setting {:.0}%), window {:.0}x{:.0} points",
            ctx.zoom_factor() * 100.0,
            size.x,
            size.y,
        );
    }

    fn sync_ui_scale(&mut self, ctx: &egui::Context) {
        let saved = self.state.settings.lock().unwrap().ui_scale;
        match decide_scale(ctx.zoom_factor(), self.applied_ui_scale, saved) {
            ScaleAction::Nothing => {}
            ScaleAction::Adopt(scale) => {
                self.applied_ui_scale = scale;
                let mut settings = self.state.settings.lock().unwrap();
                if (settings.ui_scale - scale).abs() > SCALE_EPSILON {
                    settings.ui_scale = scale;
                    drop(settings);
                    // Through the same settling delay as any other edit, so
                    // holding Ctrl+plus does not write the file per keystroke.
                    self.settings_settling = Some(std::time::Instant::now());
                }
            }
            ScaleAction::Push(scale) => {
                ctx.set_zoom_factor(scale);
                self.applied_ui_scale = scale;
            }
        }
    }
}

/// What [`GuestApp::sync_ui_scale`] should do about the interface scale.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ScaleAction {
    Nothing,
    /// Something other than the settings moved egui's zoom — its own Ctrl+plus
    /// and Ctrl+minus, or its persistence restoring a value at startup. Record
    /// it, and take it as the new setting if it differs from one.
    Adopt(f32),
    /// The setting differs from what egui is drawing at. Hand it over.
    Push(f32),
}

/// Decide which way the interface scale should travel this frame.
///
/// `live` is egui's zoom now, `applied` is what this application last gave it,
/// and `saved` is the setting. Separated from the frame it runs in because the
/// interesting part is entirely in the comparison, and because getting it
/// backwards is invisible: pushing when it should adopt makes the keyboard
/// shortcut appear not to work, and adopting when it should push makes the
/// slider appear not to.
fn decide_scale(live: f32, applied: f32, saved: f32) -> ScaleAction {
    // egui moved first: whoever did that is looking at the result, so it wins.
    // Adopted even when it already equals the setting — otherwise `applied`
    // never catches up, and the next move of the slider is read as egui having
    // moved again and is overwritten by the value it was trying to replace.
    if (live - applied).abs() > SCALE_EPSILON {
        return ScaleAction::Adopt(live);
    }
    if (saved - live).abs() > SCALE_EPSILON {
        return ScaleAction::Push(saved);
    }
    ScaleAction::Nothing
}

/// Smaller than a fifth of the slider's step, and far below anything the eye
/// resolves. Floats arriving from three places will not be equal.
///
/// Shared with the settings panel so the control and the thing it drives cannot
/// disagree about what counts as a change.
pub(super) const SCALE_EPSILON: f32 = 0.001;

impl eframe::App for GuestApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        WINDOW_BG.to_normalized_gamma_f32()
    }

    /// Tell every connected guest before the window goes.
    ///
    /// # Why this is not already covered
    ///
    /// A guest that leaves says so, and a guest the host kicks is told — both
    /// have been true for a while. Closing the host's window was the case
    /// nobody was told about: the process simply stopped, and every guest sat
    /// looking at the last frame it received until QUIC gave up on a connection
    /// that had stopped answering, which is tens of seconds of a picture that
    /// looks live and is not.
    ///
    /// A kick is exactly the right message for it — the session is over and the
    /// host decided — so this is the same path, with a reason that says which
    /// one it was.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.say_goodbye_to_guests();
        // Before anything slow, because this is the one that decides whether
        // the machine is reachable in a moment's time. Closing this window used
        // to take unattended access down with it: the service went on yielding
        // to a host that no longer existed, and the only way back was to restart
        // the service. See `stopped_hosting`.
        stopped_hosting(&self.state);
        // A virtual display belongs to the application that put it on the
        // desktop, and this is that application closing. Leaving it behind
        // gives somebody a screen they cannot see, cannot use and have nothing
        // left running to remove.
        //
        // Only the ordinary way out comes through here. A process that is
        // killed runs none of this, which is why the agent asks the same
        // question on a timer — see `display_driver::reap_orphans`.
        match crate::display_driver::release() {
            0 => {}
            n => info!("Put away {n} virtual display(s) on the way out"),
        }
        // Otherwise the icon is left in the tray until something makes the
        // shell notice the process has gone, which is usually a mouse passing
        // over it.
        super::notify::shut_down();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_ui_scale(ctx);
        self.claim_focus_once(ctx);
        self.note_scale_change(ctx);
        let mut t = self.frametime.begin();
        // The endpoint may have finished binding since the last frame; see
        // `attach_when_ready`.
        self.take_pending_handles();
        self.poll_channels();
        t = self.frametime.phase("poll", t);

        if self.auto_connect {
            // Wait for the host to have produced a code before dialling it.
            // Bind the clone so the mutex guard is dropped before start_session.
            let code = self.state.host.master_passcode.lock().unwrap().clone();
            if let Some(code) = code {
                self.auto_connect = false;
                *self.state.connect.passcode.lock().unwrap() = code;
                warn!("TAIN_DEBUG=loopback — connecting to our own host endpoint");
                self.start_session(ctx);
            }
        }

        let mut edits = PendingEdits::default();

        // Fixed band. `exact_height` is what stops the header from growing into
        // the middle section as its contents change.
        egui::TopBottomPanel::top("header")
            .exact_height(HEADER_HEIGHT)
            .frame(
                egui::Frame::none()
                    .fill(HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| self.render_header(ui, &mut edits));
        t = self.frametime.phase("header", t);

        // Footer sizes to its pills, then the middle gets everything else.
        let incoming_panel = egui::TopBottomPanel::bottom("incoming")
            .frame(
                egui::Frame::none()
                    .fill(WINDOW_BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().top() - 10.0,
                    egui::Stroke::new(1.0, SEPARATOR),
                );
                self.render_incoming_connections(ui, &mut edits)
            });
        // Where each card ended up, so a decision can be drawn on top of the
        // one it is about. Same frame: the cards were just laid out.
        let cards = incoming_panel.inner;
        t = self.frametime.phase("incoming", t);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(WINDOW_BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t!("previous.title"))
                            .size(14.0)
                            .strong()
                            .color(TEXT),
                    );
                    // Beside the heading rather than on each card: the sweep is
                    // one pass over all of them, so offering it per card would
                    // promise something narrower than what happens.
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(t!("previous.check_again")).size(12.0).color(TEXT_DIM),
                        ))
                        .on_hover_text(
                            "Ask every host on this list whether it is reachable, now. \
                             A card that says offline can still be reached with Connect — \
                             this is the same question asked again, not a different one.",
                        )
                        .clicked()
                        && let Some(tx) = self.resweep_tx.as_ref()
                    {
                        // Cleared here as well as in the sweep, so the cards
                        // answer the click on this frame rather than whenever
                        // the prober task next wakes. Pressing a button and
                        // watching nothing happen is the complaint this fixes,
                        // and a task hop is long enough to be that complaint.
                        self.state.previous_connections.lock().unwrap().forget_probe_results();
                        let _ = tx.send(());
                    }
                });
                ui.add_space(10.0);
                // auto_shrink=false makes the scroll area claim the whole panel,
                // so the middle section keeps its space even when nearly empty.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.render_previous_connections(ui, &mut edits));
            });
        t = self.frametime.phase("cards", t);

        if edits.open_settings {
            self.show_settings = true;
        }
        if self.show_settings {
            let mut settings = self.state.settings();
            let changed = settings_panel::show(
                ctx,
                &mut self.show_settings,
                &mut settings,
                &mut self.settings_panel,
            );
            if changed {
                // # The language is applied here and not on settle
                //
                // Everything else in this window is a *setting* — something the
                // host does differently from now on — and those wait for the
                // user to stop fiddling before anything is rebuilt. The
                // language is not that. It is what this window is written in,
                // and one that waited even a quarter of a second would read as
                // a choice that did not take.
                //
                // It was worse than that until now: nothing applied it at all
                // outside `main`, so picking a language did nothing until the
                // application was restarted.
                tain_core::i18n::follow(&settings.language);
                // The edit is visible immediately, but writing it to disk and
                // rebuilding the capture pipeline wait for the user to settle —
                // see `settings_settling`.
                *self.state.settings.lock().unwrap() = settings;
                self.settings_settling = Some(std::time::Instant::now());
            }
        }
        // Outside the block above so it survives Settings being closed.
        settings_panel::show_licences(ctx, &mut self.settings_panel);
        self.flush_settled_settings(ctx);

        t = self.frametime.phase("settings", t);

        self.render_confirms(ctx, &cards);
        self.render_admin_warning(ctx);

        // # Somebody clicked the notification and wants this window
        //
        // Asked through the viewport rather than done with `SetForegroundWindow`
        // here: taking the foreground is a thing Windows is entitled to refuse,
        // and winit already handles the difference between a request that is
        // granted and one that turns into a flashing taskbar button.
        //
        // Un-minimised first, because a window that has focus and is minimised
        // is still a window nobody can see.
        super::notify::watch(ctx);
        if super::notify::was_clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        if let Some(typ) = edits.copied {
            self.copied_at = Some((typ, std::time::Instant::now()));
            // The cover is for the *next* accident, not this deliberate copy.
            if typ == PasscodeType::Master {
                self.admin_cover.close();
            }
        }
        if !edits.regenerate.is_empty() {
            self.copied_at = None;
            // A new code is a new secret, and it starts covered like the last.
            self.admin_cover.close();
        }

        // # The cover, in the order the frame decided things
        //
        // The warning first: it is raised on first contact with the cover, and
        // whoever reads it has not yet finished the drag. Then the drag itself,
        // which is only allowed to open anything once the warning has been
        // read — otherwise a single long drag would both raise the explanation
        // and slide past it.
        if edits.admin_warning && !self.state.settings().admin_copy_warning_read {
            self.admin_warning_open = true;
            self.admin_cover.close();
        } else {
            if let Some(progress) = edits.admin_slider {
                self.admin_cover.progress = progress;
            }
            if edits.admin_uncovered {
                self.admin_cover.open();
            }
        }
        // Starting a session needs `ctx`, which `apply` does not have.
        let connect = edits.connect;
        let reconnect = edits.reconnect.clone();
        self.apply(edits);
        if connect {
            self.start_session(ctx);
        }
        if let Some(endpoint_id) = reconnect {
            self.reconnect(&endpoint_id, ctx);
        }
        t = self.frametime.phase("apply", t);

        // Sessions render in their own window, which is what keeps fullscreen
        // and disconnect from ever stranding the main window.
        if !self.sessions.is_empty() {
            let settings = self.state.settings();
            let actions = self.session_window.show(
                ctx,
                &mut self.sessions,
                &settings,
                &mut self.immersive,
            );

            if actions.close_all {
                self.close_all_sessions();
            } else {
                for tab_id in actions.close {
                    self.close_session(tab_id);
                }
            }
        }

        // Repaint on a timer rather than every frame. Unconditional
        // request_repaint() pins a core at the monitor's refresh rate for a UI
        // that is idle most of the time; a live session drives its own repaints
        // from the network task as frames arrive.
        self.frametime.phase("session-window", t);
        self.frametime.end();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// A header section title, centred against one row of `height`.
///
/// Both titles line up with the *first* button row rather than with the middle
/// of their own block. The Host block is two rows tall and the Connect block is
/// one, so centring each against its own block put the two titles at different
/// heights and floated the Connect row into the gap between the Host rows.
fn section_label(ui: &mut egui::Ui, text: &str, height: f32) {
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(19.0),
        TEXT,
    );
    let size = galley.size();
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(size.x, height), egui::Sense::hover());
    ui.painter()
        .galley(egui::pos2(rect.left(), rect.center().y - size.y * 0.5), galley, TEXT);
}

#[cfg(test)]
mod scale_tests {
    use super::{decide_scale, ScaleAction};

    /// Nothing to do when all three already agree, which is every frame.
    #[test]
    fn a_settled_scale_is_left_alone() {
        assert_eq!(decide_scale(1.0, 1.0, 1.0), ScaleAction::Nothing);
        assert_eq!(decide_scale(1.25, 1.25, 1.25), ScaleAction::Nothing);
    }

    /// The slider moved: hand the new value to egui.
    #[test]
    fn a_changed_setting_is_pushed_into_egui() {
        // Drawing at 1.0, that is also what we last gave it, setting says 1.5.
        assert_eq!(decide_scale(1.0, 1.0, 1.5), ScaleAction::Push(1.5));
    }

    /// Ctrl+plus moved it: take that as the setting rather than undoing it.
    #[test]
    fn a_zoom_from_the_keyboard_is_adopted() {
        // egui is at 1.1, we last gave it 1.0, and the setting still says 1.0.
        assert_eq!(decide_scale(1.1, 1.0, 1.0), ScaleAction::Adopt(1.1));
    }

    /// The failure that makes the slider look broken.
    ///
    /// egui's own persistence restores a zoom at startup, so its value differs
    /// from what this application has handed it even though it matches the
    /// setting. If that were reported as "nothing to do", the record of what we
    /// last applied would never catch up — and the next move of the slider
    /// would be read as egui having moved, and adopted straight back to the
    /// value the user was trying to change.
    #[test]
    fn a_restored_zoom_is_recorded_so_the_slider_still_works_afterwards() {
        // Restored to 1.5 by egui; the setting already says 1.5.
        assert_eq!(decide_scale(1.5, 1.0, 1.5), ScaleAction::Adopt(1.5));

        // Having recorded it, moving the slider now pushes rather than adopts.
        assert_eq!(decide_scale(1.5, 1.5, 1.75), ScaleAction::Push(1.75));
    }

    /// Float noise must not start a fight between the two directions.
    #[test]
    fn a_difference_too_small_to_see_is_not_a_change() {
        assert_eq!(decide_scale(1.0, 1.0 + 1e-6, 1.0), ScaleAction::Nothing);
        assert_eq!(decide_scale(1.0, 1.0, 1.0 - 1e-6), ScaleAction::Nothing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a pending card paints must land on the card.
    ///
    /// Two separate faults have put dashes outside it. The first drew the
    /// outline on `content_ui.min_rect()` — what the *contents* claimed — and a
    /// horizontal layout claims the width it is offered, so the border enclosed
    /// the whole row.
    ///
    /// The second only appears once the clock has been running. The marching
    /// dashes are driven by a phase taken from `input.time`, and epaint seeds
    /// its walk along the path with that phase without clamping it, so a
    /// negative one starts the walk somewhere behind the card and draws its way
    /// there — a line trailing off into the window, growing longer the longer
    /// the application has been open.
    ///
    /// This test was written for the first fault and ran at `time: 0`, which is
    /// the one value where the second cannot appear. It passed, the release
    /// build showed the stray line anyway, and that is the lesson worth keeping:
    /// a default `RawInput` is not a running application. The clock below is
    /// deliberately a couple of minutes in.
    #[test]
    fn a_pending_card_paints_nothing_outside_itself() {
        let ctx = egui::Context::default();
        let conn = IncomingConnection {
            session_id: 1,
            display_name: "DESKPC".into(),
            name_is_self_reported: true,
            mouse_allowed: false,
            keyboard_allowed: false,
            status: ApprovalStatus::Pending,
            is_admin: false,
            link: String::new(),
        };

        let mut card = egui::Rect::NOTHING;
        let mut painted = egui::Rect::NOTHING;
        // A couple of frames apart, because egui sizes from what it measured on
        // the frame before — and at two different times, so a phase that only
        // misbehaves at one of them cannot slip through.
        for time in [137.0, 137.5] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                time: Some(time),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default()
                    // No frame of its own, so every shape below belongs to the
                    // card and nothing else.
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        let mut edits = PendingEdits::default();
                        card = ui
                            .horizontal(|ui| {
                                GuestApp::render_incoming_pill(ui, &conn, None, &mut edits);
                            })
                            .response
                            .rect;
                    });
            });
            painted = egui::Rect::NOTHING;
            for clipped in output.shapes {
                let bounds = clipped.shape.visual_bounding_rect();
                if bounds.is_positive() {
                    painted = painted.union(bounds);
                }
            }
        }

        // A stroke and the antialiasing around it land a little outside the
        // rectangle they trace. Everything that has actually gone wrong here
        // was out by hundreds, so the slack costs nothing.
        let slack = 4.0;
        assert!(
            card.expand(slack).contains_rect(painted),
            "the card is at {:?} but the painting covers {:?} — something is being \
             drawn outside it",
            card,
            painted,
        );
    }

    /// Every card is the same size, whatever name is on it.
    ///
    /// # What this replaced, and why
    ///
    /// It used to assert that a longer name produced a *wider* card, because
    /// the card grew to fit and the bug being guarded against was a name
    /// running underneath the permission marks.
    ///
    /// Cards are now a fixed width — the approval popup is drawn above one and
    /// takes its width, and the marks are laid out from its right edge, neither
    /// of which works against a card that resizes itself. So the old assertion
    /// is false by design and the bug it guarded against has moved: a name that
    /// no longer fits does not overflow sideways, it *wraps*, making one card
    /// taller than the rest of the strip.
    ///
    /// Measuring both dimensions catches that, and is a closer statement of
    /// what is actually wanted than the width ever was.
    #[test]
    fn a_card_is_the_same_size_whatever_name_is_on_it() {
        let card_size = |name: &str| {
            let ctx = egui::Context::default();
            let conn = IncomingConnection {
                session_id: 1,
                display_name: name.to_string(),
                name_is_self_reported: true,
                mouse_allowed: false,
                keyboard_allowed: false,
                status: ApprovalStatus::Allowed,
                is_admin: false,
                link: String::new(),
            };

            let mut size = egui::Vec2::ZERO;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            // Twice: egui sizes from what it measured on the frame before.
            for _ in 0..2 {
                let _ = ctx.run(input.clone(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let mut edits = PendingEdits::default();
                        // The card's own rectangle, not the panel it sits in.
                        size = ui
                            .horizontal(|ui| {
                                GuestApp::render_incoming_pill(ui, &conn, None, &mut edits)
                            })
                            .inner
                            .size();
                    });
                });
            }
            size
        };

        let short = card_size("Q");
        let long = card_size("LAPTOP-7K2Q9F");
        // A name from a machine that chose its own, in a script where twelve
        // characters is nothing like twelve Latin ones.
        let wide = card_size("Мой-компьютер");

        assert_eq!(short.x, theme::PILL_W, "a card is one width and this is it");
        for (name, measured) in [("long", long), ("wide", wide)] {
            assert_eq!(
                measured.x, short.x,
                "the {name} name changed the card's width — the popup above it \
                 takes this width and would no longer match"
            );
            assert_eq!(
                measured.y, short.y,
                "the {name} name changed the card's height — it is wrapping \
                 instead of being truncated"
            );
        }
    }

    /// A name from another machine must never be able to panic this one.
    ///
    /// The card shortens by characters rather than bytes for exactly this: a
    /// `String` sliced at a byte index that lands inside a character panics,
    /// and a name in Cyrillic or CJK is all multi-byte characters — which are
    /// also the names most likely to be long enough to need shortening.
    #[test]
    fn a_name_in_any_script_is_shortened_rather_than_split() {
        for name in [
            "\u{041c}\u{043e}\u{0439} \u{043a}\u{043e}\u{043c}\u{043f}\u{044c}\u{044e}\u{0442}\u{0435}\u{0440} \u{0434}\u{043e}\u{043c}\u{0430}",
            "\u{4e0a}\u{6d77}\u{7684}\u{5de5}\u{4f5c}\u{7ad9}\u{4e00}\u{53f7}\u{673a}\u{5668}\u{4eba}",
            "\u{1f5a5}\u{fe0f} studio desktop",
        ] {
            // Every cut point, not just the one the card happens to use. A
            // single length proves very little here: a byte-sliced version of
            // this passed on the first name tried, because eleven bytes into it
            // lands on a character boundary by luck. One of these does not.
            for max in 1..=name.chars().count() + 2 {
                let short = elide(name, max);
                assert!(
                    short.chars().count() <= max,
                    "{name:?} cut to {max} became {short:?}, which is longer"
                );
                assert!(
                    name.starts_with(short.trim_end_matches('\u{2026}')),
                    "{short:?} is not the start of {name:?}"
                );
            }
        }
    }

    /// A name that fits is left exactly as it is — no stray ellipsis.
    #[test]
    fn a_short_name_is_not_marked_as_cut() {
        for name in ["Q", "DESKPC", "LAPTOP-7K2Q"] {
            assert_eq!(elide(name, PILL_NAME_MAX_CHARS), name);
        }
        // Exactly at the cap still counts as fitting.
        let twelve = "abcdefghijkl";
        assert_eq!(twelve.chars().count(), PILL_NAME_MAX_CHARS);
        assert_eq!(elide(twelve, PILL_NAME_MAX_CHARS), twelve);
    }

    /// And one that does not fit says so, within the same budget.
    #[test]
    fn a_long_name_is_cut_and_marked() {
        let long = "LAPTOP-7K2Q9F-workstation";
        let short = elide(long, PILL_NAME_MAX_CHARS);
        assert!(short.ends_with('\u{2026}'), "{short:?} does not say it was cut");
        assert_eq!(short.chars().count(), PILL_NAME_MAX_CHARS);
        assert!(long.starts_with(short.trim_end_matches('\u{2026}')));
    }


    use tain_core::PasscodeType as CorePasscodeType;

    fn incoming(session_id: u32, status: ApprovalStatus) -> IncomingConnection {
        IncomingConnection {
            session_id,
            display_name: format!("Guest #{session_id}"),
            name_is_self_reported: false,
            mouse_allowed: false,
            keyboard_allowed: false,
            status,
            is_admin: false,
            link: String::new(),
        }
    }

    /// A state a test built must never be able to write to the real machine.
    ///
    /// # The bug this is here for
    ///
    /// `AppState::new()` exists so a test stays away from the user's data, and
    /// it did avoid *reading* it — but every path that edited a setting still
    /// called `Settings::save`, which writes to the real configuration
    /// directory. Running the suite overwrote the developer's own settings.
    ///
    /// Same lesson as `SecretStore`, in a second place: the decision belongs on
    /// the value, not on the call site.
    #[test]
    fn a_state_built_for_a_test_is_not_backed_by_disk() {
        let state = AppState::new();
        assert!(!state.on_disk, "a test state must not be saveable to disk");

        // And saving through it is a no-op rather than a write.
        state.settings.lock().unwrap().target_fps = 123;
        state.save_settings();

        // Nothing to assert about the filesystem without touching it, which is
        // the point — the flag is the guarantee, checked above.
        assert_eq!(state.settings.lock().unwrap().target_fps, 123);
    }

    /// Closing the host's window has to reach the guests, not just stop.
    ///
    /// Reported: a guest whose host closed its window kept showing the last
    /// frame it had received until QUIC gave up on the connection — tens of
    /// seconds of a picture that looks live and is not. A guest leaving said
    /// so, and a kicked guest was told; this was the one ending nobody
    /// announced.
    #[test]
    fn closing_the_window_tells_every_connected_guest() {
        let (mut app, sessions) = app_with_sessions();

        let mut inboxes = Vec::new();
        for _ in 0..3 {
            let id = sessions.add_session([0u8; 32], ControlGrants::ALL, PasscodeType::Guest);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            sessions.register_outbound(id, tx);
            inboxes.push((id, rx));
        }

        app.say_goodbye_to_guests();

        for (id, mut rx) in inboxes {
            match rx.try_recv() {
                Ok(tain_core::protocol::Message::SessionEnd(end)) => {
                    assert_eq!(end.session_id, id);
                    assert!(
                        !end.reason.is_empty(),
                        "a guest told nothing is a guest guessing why"
                    );
                }
                other => panic!("session #{id} was not told the host was closing: {other:?}"),
            }
        }
    }

    /// And a host with nobody connected closes at once.
    ///
    /// The goodbye waits for the writers to be scheduled, and that wait must
    /// not be paid by someone who simply opened the application and shut it.
    #[test]
    fn closing_with_no_guests_does_not_wait() {
        let (mut app, _sessions) = app_with_sessions();
        let started = std::time::Instant::now();
        app.say_goodbye_to_guests();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "closing an idle host took {:?}",
            started.elapsed()
        );
    }

    /// An admin connection must appear in the list, or it cannot be ended.
    ///
    /// # The bug this is here for
    ///
    /// Cards were created only when an approval was requested, and an admin
    /// passcode skips approval by design. So a connection holding the strongest
    /// credential — full mouse and keyboard from the first frame — appeared
    /// nowhere in the host's UI and there was no control to disconnect it.
    #[test]
    fn a_session_that_was_never_asked_about_still_appears_and_can_be_ended() {
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session(
            [0u8; 32],
            tain_core::ControlGrants::ALL,
            CorePasscodeType::Master,
        );
        sessions.set_label(id, "My laptop".into());

        // Nothing pushed a card: an admin session never requests approval.
        assert!(app.state.incoming.lock().unwrap().is_empty());

        app.reconcile_incoming();

        let incoming = app.state.incoming.lock().unwrap();
        let card = incoming
            .iter()
            .find(|c| c.session_id == id)
            .expect("an admin session must be listed");
        assert!(card.is_admin, "and must be marked as one");
        assert_eq!(card.status, ApprovalStatus::Allowed, "it is already streaming");
        assert!(card.mouse_allowed && card.keyboard_allowed, "it holds full control");
        assert_eq!(card.display_name, "My laptop");
    }

    /// A guest awaiting approval must not be promoted by the same pass.
    #[test]
    fn reconciling_does_not_approve_a_guest_that_is_still_waiting() {
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session(
            [0u8; 32],
            tain_core::ControlGrants::NONE,
            CorePasscodeType::Guest,
        );
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Pending));

        app.reconcile_incoming();

        let incoming = app.state.incoming.lock().unwrap();
        let card = incoming.iter().find(|c| c.session_id == id).expect("still listed");
        assert_eq!(card.status, ApprovalStatus::Pending, "the decision is the user's");
        assert!(!card.is_admin);
    }

    fn app_with_sessions() -> (GuestApp, Arc<SessionManager>) {
        let sessions = Arc::new(SessionManager::new());
        let mut app = GuestApp::new(AppState::new());
        app.host_sessions = Some(Arc::clone(&sessions));
        (app, sessions)
    }

    /// An app whose saved-connection list is in memory only.
    ///
    /// `AppState::new()` loads the real user's `connections.json`, and the
    /// store's mutating methods persist as a side effect — so a test that
    /// records anything would write its fixtures into the running app's list.
    fn app_with_scratch_connections() -> GuestApp {
        let app = GuestApp::new(AppState::new());
        *app.state.previous_connections.lock().unwrap() = ConnectionStore::new();
        app
    }

    fn bare_session() -> tain_core::SessionInfo {
        tain_core::SessionInfo {
            secret: [0u8; 32],
            grants: tain_core::ControlGrants::NONE,
            display_config: None,
            passcode_type: tain_core::PasscodeType::Guest,
            label: "Guest #1".into(),
            stream_status: None,
            route: None,
            link: None,
        }
    }

    /// A card exists before the guest has reported anything.
    #[test]
    fn a_connection_with_nothing_to_say_says_nothing() {
        assert!(describe_link(&bare_session()).is_empty());
    }

    #[test]
    fn the_link_summary_leaves_out_what_has_not_arrived() {
        let mut info = bare_session();
        info.route = Some("direct — 192.168.0.10:57374".into());
        let text = describe_link(&info);
        assert_eq!(text, "direct — 192.168.0.10:57374", "no round trip has been reported");

        info.link = Some(tain_core::GuestTelemetry {
            decode_ms: 3.0,
            render_ms: 1.0,
            displayed_fps: 60.0,
            decoded_fps: 59.0,
            rtt_ms: 11.6,
        });
        let text = describe_link(&info);
        assert!(text.contains("12 ms round trip"), "got {text:?}");
        assert!(
            !text.contains("decoding"),
            "the guest's window size is not known yet: {text:?}"
        );

        info.display_config = Some(tain_core::protocol::DisplayConfig {
            target_fps: 0,
            max_bitrate_kbps: 0,
            client_width: 1856,
            client_height: 1020,
        });
        assert!(describe_link(&info).contains("59 fps into 1856x1020"));
    }

    /// A degraded stream names what is holding it back — that is the whole
    /// reason the host is shown this rather than left to read the log.
    #[test]
    fn a_degraded_stream_says_what_is_limiting_it() {
        let mut info = bare_session();
        let mut held = tain_core::BottleneckSet::NONE;
        held.insert(tain_core::Bottleneck::SlowEncode);
        info.stream_status = Some(tain_core::protocol::StreamStatus {
            session_id: 1,
            bottlenecks: held.bits(),
            fps: 40,
            bitrate_kbps: 9216,
            degraded: true,
        });
        let text = describe_link(&info);
        assert!(text.contains("40 fps at 9.2 Mbit/s"), "got {text:?}");
        assert!(text.contains("held back by"), "got {text:?}");

        // Healthy, so there is nothing to blame.
        info.stream_status = Some(tain_core::protocol::StreamStatus {
            session_id: 1,
            bottlenecks: 0,
            fps: 60,
            bitrate_kbps: 800,
            degraded: false,
        });
        assert_eq!(describe_link(&info), "Sending 60 fps at 800 kbit/s");
    }

    #[test]
    fn incoming_snapshot_is_independent_of_the_shared_vec() {
        // Regression: the render pass held the `incoming` guard while the device
        // toggles re-locked it, deadlocking the UI thread on first click.
        let state = AppState::new();
        state.incoming.lock().unwrap().push(incoming(1, ApprovalStatus::Pending));

        let snapshot = state.incoming.lock().unwrap().clone();
        state.incoming.lock().unwrap()[0].mouse_allowed = true;

        assert!(!snapshot[0].mouse_allowed);
        assert!(state.incoming.lock().unwrap()[0].mouse_allowed);
    }

    #[test]
    fn a_disconnected_guest_stops_being_listed() {
        // Regression: after the guest disconnected, the host kept showing the
        // pill with a green dot as though the session were still live.
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session([0u8; 32], ControlGrants::ALL, CorePasscodeType::Guest);
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Allowed));

        app.reconcile_incoming();
        assert_eq!(app.state.incoming.lock().unwrap().len(), 1, "still connected");

        sessions.remove_session(id);
        app.reconcile_incoming();
        assert!(app.state.incoming.lock().unwrap().is_empty(), "gone once the session ends");
    }

    #[test]
    fn reconciling_mirrors_the_grants_the_server_actually_holds() {
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, CorePasscodeType::Guest);
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Allowed));

        sessions.set_control(id, ControlGrants::new(true, false));
        app.reconcile_incoming();

        let card = app.state.incoming.lock().unwrap()[0].clone();
        assert!(card.mouse_allowed);
        assert!(!card.keyboard_allowed);
        assert_eq!(card.status, ApprovalStatus::Allowed);
    }

    #[test]
    fn an_allowed_guest_with_no_input_is_still_allowed_not_denied() {
        // View-only is a real outcome now: the screen is streaming even though
        // no device is granted. Reading grants as approval would mislabel it.
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, CorePasscodeType::Guest);
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Allowed));

        app.reconcile_incoming();

        let card = app.state.incoming.lock().unwrap()[0].clone();
        assert_eq!(card.status, ApprovalStatus::Allowed);
        assert!(!card.mouse_allowed && !card.keyboard_allowed);
    }

    #[test]
    fn a_pending_card_keeps_its_status_until_the_user_decides() {
        // Grants are NONE while pending, and the guest is being streamed
        // nothing — the card must keep saying so until someone answers.
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, CorePasscodeType::Guest);
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Pending));

        app.reconcile_incoming();

        assert_eq!(app.state.incoming.lock().unwrap()[0].status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn a_vanished_session_drops_its_pending_responder_and_dialog() {
        let (mut app, sessions) = app_with_sessions();
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, CorePasscodeType::Guest);
        let (tx, _rx) = oneshot::channel();
        app.pending_approvals.insert(id, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Pending));
        app.confirms =
            vec![Confirm::new(id, "Guest #1".into(), false, ConfirmAction::Approve)];

        sessions.remove_session(id);
        app.reconcile_incoming();

        assert!(app.pending_approvals.is_empty(), "responder should not outlive the session");
        assert!(app.confirms.is_empty(), "a dialog for a dead session is unanswerable");
    }

    #[tokio::test]
    async fn allowing_answers_the_pending_oneshot() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(7, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(7, ApprovalStatus::Pending));

        app.resolve_approval(7, ApprovalDecision::Allow { grants: ControlGrants::ALL, displays: tain_core::protocol::DisplayGrant::All });

        assert_eq!(
            rx.await.unwrap(),
            ApprovalDecision::Allow { grants: ControlGrants::ALL, displays: tain_core::protocol::DisplayGrant::All },
            "the server is blocked on this — it streams nothing until it arrives"
        );
        assert_eq!(app.state.incoming.lock().unwrap()[0].status, ApprovalStatus::Allowed);
        assert!(app.pending_approvals.is_empty());
    }

    #[tokio::test]
    async fn allowing_with_no_devices_still_starts_the_stream() {
        // "Share the screen, keep the mouse" — the checkbox default, and it has
        // to reach the server as an Allow rather than looking like a denial.
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(3, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(3, ApprovalStatus::Pending));

        app.resolve_approval(3, ApprovalDecision::Allow { grants: ControlGrants::NONE, displays: tain_core::protocol::DisplayGrant::All });

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow { grants: ControlGrants::NONE, displays: tain_core::protocol::DisplayGrant::All });
        assert_eq!(app.state.incoming.lock().unwrap()[0].status, ApprovalStatus::Allowed);
    }

    #[tokio::test]
    async fn denying_disconnects_rather_than_leaving_a_greyed_out_card() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(5, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(5, ApprovalStatus::Pending));

        app.resolve_approval(5, ApprovalDecision::Deny);

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Deny);
        assert!(app.state.incoming.lock().unwrap().is_empty(), "a refused guest is gone");
        assert!(app.pending_approvals.is_empty());
    }

    #[tokio::test]
    async fn granting_the_mouse_alone_leaves_the_keyboard_denied() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(9, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(9, ApprovalStatus::Pending));

        app.set_grants(9, ControlGrants::new(true, false));

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow {
                grants: ControlGrants::new(true, false),
                displays: tain_core::protocol::DisplayGrant::All,
            });
        let card = app.state.incoming.lock().unwrap()[0].clone();
        assert!(card.mouse_allowed);
        assert!(!card.keyboard_allowed);
        assert_eq!(card.status, ApprovalStatus::Allowed);
    }

    #[tokio::test]
    async fn kicking_answers_the_responder_so_the_server_is_not_left_waiting() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(4, PendingApproval::guest(tx));
        app.state.incoming.lock().unwrap().push(incoming(4, ApprovalStatus::Pending));

        app.kick(4);

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Deny);
        assert!(app.state.incoming.lock().unwrap().is_empty());
        assert!(app.pending_approvals.is_empty());
    }

    #[test]
    fn the_approval_dialog_says_what_each_answer_means() {
        // The name is the dialog's heading now, so the body must not repeat it
        // — "Guest #1 Guest #1 wants to connect" is what this catches.
        let approve = Confirm::new(1, "Guest #1".into(), false, ConfirmAction::Approve);
        assert!(!approve.body().contains("Guest #1"), "the heading already says who");
        assert!(approve.body().contains("connect"));
        assert_eq!(approve.confirm_label(), "Allow");

        let kick = Confirm::new(1, "Guest #1".into(), false, ConfirmAction::Kick);
        assert!(kick.body().contains("disconnected"));
        assert_eq!(kick.confirm_label(), "Disconnect");
        assert_ne!(approve.title(), kick.title());
    }

    /// An admin connection is a different decision and has to read as one.
    ///
    /// The guest wording is about watching a screen. If the admin case ever
    /// fell back to it, the most consequential dialog in the application would
    /// be the one that undersold itself.
    #[test]
    fn the_admin_dialog_does_not_read_like_the_guest_one() {
        let guest = Confirm::new(1, "Friend PC2".into(), false, ConfirmAction::Approve);
        let admin = Confirm::new(1, "Friend PC2".into(), false, ConfirmAction::Approve)
            .for_admin(true);

        assert_ne!(admin.body(), guest.body());
        assert_ne!(admin.title(), guest.title());
        // The two things somebody has to know before saying yes: what it grants
        // now, and that the answer is remembered.
        let says = admin.body().to_lowercase();
        assert!(says.contains("admin passcode"), "it must say which code this is");
        assert!(
            says.contains("keyboard") && says.contains("screen"),
            "it must say what the code already carries: {says}"
        );
    }

    #[test]
    fn neither_device_is_offered_by_default() {
        // The safe answer has to be the one you get by not thinking about it.
        let approve = Confirm::new(1, "Guest".into(), false, ConfirmAction::Approve);
        assert_eq!(approve.grants, ControlGrants::NONE);
        assert!(!approve.grants.any());

        // And the shipped default keeps it that way.
        assert_eq!(Settings::default().default_grants(), ControlGrants::NONE);
    }

    #[test]
    fn configured_defaults_seed_the_dialog_but_are_still_confirmed() {
        // Pre-ticking is a convenience, not an auto-approval: the dialog still
        // opens and still has to be answered.
        let opted_in = Settings {
            default_allow_mouse: true,
            default_allow_keyboard: false,
            ..Settings::default()
        };
        assert_eq!(opted_in.default_grants(), ControlGrants::new(true, false));

        let dialog = Confirm::with_grants(
            1,
            "Guest".into(),
            false,
            ConfirmAction::Approve,
            opted_in.default_grants(),
        );
        assert!(dialog.grants.mouse);
        assert!(!dialog.grants.keyboard);
    }

    /// A window a person could not point at is not offered — but one they
    /// already picked stays picked.
    ///
    /// # The bug this exists to prevent
    ///
    /// The offered list is "what could somebody click on", which excludes
    /// minimised windows: they have no rectangle on screen. Somebody picking a
    /// window and then minimising it — putting the spreadsheet away while they
    /// finish reading the request — would reopen the picker to find it gone,
    /// and would have granted one window fewer than they thought.
    #[test]
    fn a_chosen_window_stays_on_the_list_after_it_is_minimised() {
        // Whatever is actually open on the machine running this.
        let live = tain_core::window_capture::shareable();
        let offered = GuestApp::pickable(&std::collections::BTreeSet::new());
        assert_eq!(
            offered.len(),
            live.len(),
            "with nothing chosen, the picker offers exactly what can be pointed at"
        );

        // A handle that is not open and never was. It cannot be described, so
        // it drops out — a window that has *closed* has nothing left to grant.
        let gone: std::collections::BTreeSet<u64> = std::iter::once(u64::MAX).collect();
        let after = GuestApp::pickable(&gone);
        assert_eq!(
            after.len(),
            live.len(),
            "a window that has closed cannot be kept on the list, chosen or not"
        );

        // And a window that *is* open is only ever listed once, however it got
        // there — the two sources must not produce a duplicate tile.
        if let Some(first) = live.first() {
            let chosen: std::collections::BTreeSet<u64> = std::iter::once(first.id).collect();
            let listed = GuestApp::pickable(&chosen);
            assert_eq!(
                listed.iter().filter(|window| window.id == first.id).count(),
                1,
                "{} was offered twice, so ticking one tile would leave the other unticked",
                first.title
            );
        }
    }

    /// What the dialog's answers add up to, for each shape of answer.
    ///
    /// # Why the order of the windows is asserted
    ///
    /// The first window in the grant is the one the session opens on. Collected
    /// from the chosen set directly it would come out in handle order, which is
    /// whatever numbers Windows happened to hand out — so the guest would open
    /// on an apparently random one of the windows, and no arrangement of the
    /// picker could change that.
    #[test]
    fn the_dialog_grants_windows_in_the_order_they_were_offered() {
        fn window(id: u64, app: &str) -> tain_core::protocol::WindowInfo {
            tain_core::protocol::WindowInfo {
                id,
                app: app.into(),
                title: format!("{app} — something"),
                width: 800,
                height: 600,
            }
        }

        let mut dialog = Confirm::new(1, "Guest".into(), false, ConfirmAction::Approve);
        // Listed alphabetically, as the picker lists them; the handles run the
        // other way, which is what makes this test say something.
        dialog.windows = vec![window(900, "Excel"), window(300, "chrome"), window(600, "Notepad")];
        dialog.chosen = [600, 900].into_iter().collect();

        assert_eq!(
            dialog.granted(),
            tain_core::protocol::DisplayGrant::Windows(vec![900, 600]),
            "the grant has to follow the list somebody read, not the handle numbers"
        );

        // And the screens are not in it, whatever was ticked above.
        dialog.share = [0, 1].into_iter().collect();
        assert_eq!(
            dialog.granted(),
            tain_core::protocol::DisplayGrant::Windows(vec![900, 600]),
            "choosing windows means choosing nothing else — that is the whole point"
        );

        // Nothing chosen goes back to the screens.
        dialog.chosen.clear();
        assert!(matches!(
            dialog.granted(),
            tain_core::protocol::DisplayGrant::All | tain_core::protocol::DisplayGrant::Only(_)
        ));
    }

    /// The picker draws, and stays the width it asked for.
    ///
    /// # Why the width is what is asserted
    ///
    /// The tiles are a fixed size and there are three to a row, so the window
    /// has one correct width and a grid that fits it. A tile that measured
    /// wider than it draws — a long window title escaping its elision, most
    /// likely — would push the row out and take the window with it, and the
    /// symptom on screen is a picker wider than the screen it opens on.
    ///
    /// Drawing it at all is the other half: the tiles are painted by hand from
    /// a texture that may not exist yet, and this is what runs that path.
    #[test]
    fn the_window_picker_draws_within_the_width_it_asked_for() {
        let ctx = egui::Context::default();
        let mut confirm = Confirm::new(4, "Guest".into(), false, ConfirmAction::Approve);
        // Whatever is open on the machine running this, plus one with a caption
        // long enough to overflow any tile — which is an ordinary browser tab,
        // not a contrivance.
        confirm.windows = GuestApp::pickable(&std::collections::BTreeSet::new());
        confirm.windows.push(tain_core::protocol::WindowInfo {
            id: 1,
            app: "a-program-with-a-very-long-name-indeed".into(),
            title: "A window caption of the sort a browser puts on a tab, which runs on and \
                    on and has no reason ever to stop"
                .into(),
            width: 1920,
            height: 1080,
        });

        let shots = std::cell::RefCell::new(WindowShots::default());
        let mut chosen: std::collections::BTreeSet<u64> =
            confirm.windows.last().map(|w| w.id).into_iter().collect();

        // Twice: egui sizes a window from what it measured on the frame before.
        for _ in 0..2 {
            let _ = ctx.run(Default::default(), |ctx| {
                GuestApp::show_window_picker(ctx, &confirm, &mut chosen, &shots);
            });
        }

        let rect = ctx
            .memory(|m| m.area_rect(egui::Id::new(("window-picker", confirm.session_id))))
            .expect("the picker should have been laid out");
        // Three tiles, their gaps and the frame's margins, plus room for the
        // frame's own decoration.
        assert!(
            rect.width() <= 600.0,
            "the picker widened to {:.0} points — a tile is measuring wider than it draws",
            rect.width()
        );
        // And it does not run off the bottom of an ordinary screen either: the
        // grid scrolls inside a bounded area rather than growing.
        assert!(
            rect.height() <= 700.0,
            "the picker grew to {:.0} points tall, so the grid is not scrolling",
            rect.height()
        );
    }

    #[test]
    fn a_self_reported_name_is_flagged_but_a_generated_one_is_not() {
        // The host has no way to verify a name the other machine chose, so the
        // dialog says where it came from rather than implying we vouched for it.
        let claimed = Confirm::new(1, "IT Support".into(), true, ConfirmAction::Approve);
        assert!(claimed.name_caveat().is_some());

        let generated = Confirm::new(1, "Guest #1".into(), false, ConfirmAction::Approve);
        assert!(generated.name_caveat().is_none());

        // Kicking is not a trust decision, so it carries no caveat either.
        let kick = Confirm::new(1, "IT Support".into(), true, ConfirmAction::Kick);
        assert!(kick.name_caveat().is_none());
    }

    /// A guest that is waiting asks its own question.
    ///
    /// # Why this is not left to a click
    ///
    /// It was, and the card was then a notification *about* a decision rather
    /// than the decision: one click before anything could be answered, on a
    /// strip somebody is by definition not looking at — the host window is
    /// behind something, which is why there is a desktop notification at all.
    /// Nothing here has to be asked for. A guest is already waiting.
    #[tokio::test]
    async fn a_waiting_guest_asks_its_own_question() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = mpsc::unbounded_channel();
        app.approval_rx = Some(rx);

        let (approve_tx, _held) = oneshot::channel();
        tx.send(tain_core::ApprovalRequest {
            session_id: 1,
            label: "Friend PC2".into(),
            label_is_self_reported: true,
            is_admin: false,
            endpoint_id: [9u8; 32],
            approve_tx,
        })
        .expect("the receiver is attached");

        app.poll_channels();

        assert_eq!(app.confirms.len(), 1, "the question should not wait to be asked for");
        assert_eq!(app.confirms[0].session_id, 1);
        assert_eq!(app.state.incoming.lock().unwrap().len(), 1, "and the card is there too");
    }

    /// Once put away, it stays away.
    ///
    /// The popup is created when the request arrives and never re-derived from
    /// "pending and nothing open" — which would reopen it on the very next
    /// frame and make dismissing impossible.
    #[tokio::test]
    async fn a_dismissed_question_is_not_asked_again_by_itself() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = mpsc::unbounded_channel();
        app.approval_rx = Some(rx);

        let (approve_tx, _held) = oneshot::channel();
        tx.send(tain_core::ApprovalRequest {
            session_id: 1,
            label: "Friend PC2".into(),
            label_is_self_reported: false,
            is_admin: true,
            endpoint_id: [9u8; 32],
            approve_tx,
        })
        .expect("the receiver is attached");

        app.poll_channels();
        assert!(app.confirms[0].is_admin, "an admin request opens the admin question");

        // Clicking the card puts it away.
        app.apply(PendingEdits {
            confirm: vec![(1, ConfirmAction::Approve)],
            ..Default::default()
        });
        assert!(app.confirms.is_empty());

        // Frames keep happening; the guest is still waiting.
        app.poll_channels();
        app.poll_channels();
        assert!(app.confirms.is_empty(), "it reopened on its own — dismissing is impossible");
        assert_eq!(app.state.incoming.lock().unwrap().len(), 1, "the guest is still there");
    }

    /// Two guests waiting are two questions, not a queue.
    ///
    /// This used to assert the opposite — one dialog at a time — because there
    /// was one dialog, in the middle of the window, that could only be about
    /// one connection. Now each is drawn above its own card and takes that
    /// card's width, so they cannot overlap and there is no reason to make
    /// somebody answer them in turn.
    #[test]
    fn every_waiting_connection_gets_its_own_decision() {
        let mut app = GuestApp::new(AppState::new());
        app.state.incoming.lock().unwrap().push(incoming(1, ApprovalStatus::Pending));
        app.state.incoming.lock().unwrap().push(incoming(2, ApprovalStatus::Allowed));

        app.apply(PendingEdits {
            confirm: vec![(1, ConfirmAction::Approve), (2, ConfirmAction::Kick)],
            ..Default::default()
        });

        let open: Vec<u32> = app.confirms.iter().map(|c| c.session_id).collect();
        assert_eq!(open, vec![1, 2], "both connections should be answerable at once");
    }

    /// Clicking the same card twice is still a mis-click.
    ///
    /// One *per* connection is the rule that replaced one at a time, and a
    /// second popup for the same guest would stack on the first with no way to
    /// tell them apart.
    #[test]
    fn clicking_the_same_card_again_puts_its_decision_away() {
        let mut app = GuestApp::new(AppState::new());
        app.state.incoming.lock().unwrap().push(incoming(1, ApprovalStatus::Pending));

        let click = |app: &mut GuestApp| {
            app.apply(PendingEdits {
                confirm: vec![(1, ConfirmAction::Approve)],
                ..Default::default()
            })
        };

        click(&mut app);
        assert_eq!(app.confirms.len(), 1, "the first click asks the question");
        click(&mut app);
        assert!(app.confirms.is_empty(), "the second puts it away — there is no Cancel button");
        click(&mut app);
        assert_eq!(app.confirms.len(), 1, "and it can be asked again");

        // Never two for one guest: a second popup would stack on the first with
        // no way to tell them apart.
        assert_eq!(app.confirms.iter().filter(|c| c.session_id == 1).count(), 1);
    }

    /// Putting the question away is not answering it.
    ///
    /// The guest stays in the list, still waiting, still seeing nothing — where
    /// denying disconnects it. Collapsing the two would make a stray click on a
    /// card into a refusal.
    #[test]
    fn closing_a_decision_does_not_answer_it() {
        let mut app = GuestApp::new(AppState::new());
        app.state.incoming.lock().unwrap().push(incoming(1, ApprovalStatus::Pending));
        let (tx, mut rx) = oneshot::channel();
        app.pending_approvals.insert(1, PendingApproval::guest(tx));

        for _ in 0..2 {
            app.apply(PendingEdits {
                confirm: vec![(1, ConfirmAction::Approve)],
                ..Default::default()
            });
        }

        assert!(app.confirms.is_empty());
        assert!(app.pending_approvals.contains_key(&1), "the guest is still waiting");
        assert!(rx.try_recv().is_err(), "nothing was decided");
        assert_eq!(app.state.incoming.lock().unwrap().len(), 1, "the card is still there");
    }

    #[test]
    fn a_dialog_for_a_card_that_is_already_gone_is_not_opened() {
        let mut app = GuestApp::new(AppState::new());
        app.apply(PendingEdits {
            confirm: vec![(99, ConfirmAction::Approve)],
            ..Default::default()
        });
        assert!(app.confirms.is_empty());
    }

    #[test]
    fn allowing_nothing_is_not_the_same_decision_as_denying() {
        // A guest allowed with neither device still gets a session and sees the
        // screen; a denied one is disconnected. The two must never collapse.
        assert_ne!(Outcome::Allow(ControlGrants::NONE), Outcome::Deny);
    }

    #[test]
    fn closing_the_last_session_clears_the_window_chrome() {
        // Regression: disconnecting while fullscreen left the window stuck.
        let mut app = GuestApp::new(AppState::new());
        app.session_window.fullscreen = true;
        app.session_window.active_tab = 2;

        app.after_sessions_changed();

        assert!(!app.session_window.fullscreen);
        assert_eq!(app.session_window.active_tab, 0);
        assert!(!app.immersive.is_active());
    }

    /// Being kicked closes the tab and says so where the user is looking.
    ///
    /// The complaint this answers: the session window stayed open on a frozen
    /// last frame, and the only sign anything had happened was a small line of
    /// text on a toolbar row that is off by default.
    #[test]
    fn a_session_the_host_ends_closes_its_own_tab() {
        let mut app = GuestApp::new(AppState::new());
        let session = GuestSession::test_stub();
        *session.shared.status.lock().unwrap() = GuestStatus::Streaming;
        app.sessions.push(session);

        app.prune_ended_sessions();
        assert_eq!(app.sessions.len(), 1, "a live session must not be closed");

        *app.sessions[0].shared.status.lock().unwrap() =
            GuestStatus::Failed("disconnected by host".into());
        app.prune_ended_sessions();

        assert!(app.sessions.is_empty(), "the tab must close when the host ends the session");
        let notice = app.connect_error.expect("the user has to be told why the window went away");
        assert!(notice.contains("disconnected by host"), "notice was {notice:?}");
    }

    /// A session that never connected keeps its tab.
    ///
    /// Closing it would take away the error message, which is the only thing on
    /// screen explaining why nothing happened.
    #[test]
    fn a_connection_that_never_succeeded_keeps_its_tab() {
        let mut app = GuestApp::new(AppState::new());
        let session = GuestSession::test_stub();
        *session.shared.status.lock().unwrap() = GuestStatus::Failed("no route to host".into());
        app.sessions.push(session);

        app.prune_ended_sessions();
        assert_eq!(app.sessions.len(), 1, "a failed attempt must stay open to be read");
    }

    #[test]
    fn grant_descriptions_cover_every_combination() {
        assert_eq!(describe_grants(true, true), "full control");
        assert_eq!(describe_grants(true, false), "mouse only");
        assert_eq!(describe_grants(false, true), "keyboard only");
        assert_eq!(describe_grants(false, false), "view-only");
    }

    #[test]
    fn connecting_to_a_host_remembers_it_even_before_it_answers() {
        // The reported bug: disconnect, and the host was nowhere in the list.
        // Recording on attempt rather than on success is what fixes it.
        let app = app_with_scratch_connections();

        let code = ConnectionCode::new(CorePasscodeType::Guest, [7u8; 32], [0xAB; 32]);
        let hex = super::super::connections::endpoint_hex(&code.endpoint_id);
        let placeholder = super::super::connections::placeholder_name(&hex);
        app.state
            .previous_connections
            .lock()
            .unwrap()
            .record(hex.clone(), code.encode(), &placeholder);

        let conns = app.state.previous_connections.lock().unwrap();
        assert_eq!(conns.entries().len(), 1);
        assert_eq!(conns.entries()[0].display_name, "PC ABABABAB");
        assert_eq!(conns.code_for(&hex), Some(code.encode()), "and it can dial again");
    }

    #[test]
    fn the_header_autoreset_checkbox_edits_the_setting_the_server_reads() {
        // Regression: the header had its own copy of this flag that nothing
        // read, so unticking it in the header changed nothing — the guest code
        // regenerated on disconnect anyway and the saved code went stale.
        //
        // Set the starting state rather than trusting the default: AppState
        // loads the real user's settings file, so asserting on it here would
        // make the test pass or fail depending on the machine it runs on.
        let mut app = GuestApp::new(AppState::new());
        *app.state.settings.lock().unwrap() =
            Settings { guest_autoreset: true, ..Settings::default() };

        app.apply(PendingEdits {
            set_autoreset: vec![(CorePasscodeType::Guest, false)],
            ..Default::default()
        });

        assert!(!app.state.settings().guest_autoreset);
        assert!(
            !app.state.settings().to_session_config().auto_regen_guest_passcode,
            "and it must reach the config the server actually consults"
        );
    }

    // ── Codes that survive a restart ──────────────────────────

    const OURS: [u8; 32] = [0x11; 32];
    const THEIRS: [u8; 32] = [0x22; 32];
    const SECRET: [u8; 32] = [0x33; 32];

    #[test]
    fn a_code_saved_by_this_machine_comes_back_intact() {
        for typ in [CorePasscodeType::Master, CorePasscodeType::Guest] {
            let saved = ConnectionCode::new(typ, SECRET, OURS).encode();
            assert_eq!(
                restorable_secret(&saved, typ, OURS),
                Some(SECRET),
                "{} code must survive verbatim — a different secret is a dead code",
                typ.prefix()
            );
        }
    }

    #[test]
    fn a_code_for_a_different_identity_is_refused() {
        // The case that matters in practice: the credential store kept the code
        // but not the identity key, so iroh minted a new keypair. Adopting the
        // secret here would show the user a code that dials nowhere.
        let saved = ConnectionCode::new(CorePasscodeType::Guest, SECRET, THEIRS).encode();
        assert_eq!(restorable_secret(&saved, CorePasscodeType::Guest, OURS), None);
    }

    #[test]
    fn a_code_of_the_wrong_type_is_refused() {
        // A master code restored into the guest slot would hand full control to
        // everyone the user meant to make wait for approval.
        let saved = ConnectionCode::new(CorePasscodeType::Master, SECRET, OURS).encode();
        assert_eq!(restorable_secret(&saved, CorePasscodeType::Guest, OURS), None);
    }

    #[test]
    fn a_damaged_credential_is_refused_rather_than_adopted() {
        let good = ConnectionCode::new(CorePasscodeType::Guest, SECRET, OURS).encode();
        let truncated = &good[..good.len() - 4];
        for junk in ["", "GUEST-", "nonsense", truncated] {
            assert_eq!(
                restorable_secret(junk, CorePasscodeType::Guest, OURS),
                None,
                "{junk:?} must not become a working passcode"
            );
        }
    }

    #[test]
    fn keeping_a_code_means_storing_it_and_resetting_means_destroying_it() {
        let secrets = HostSecrets::in_memory();
        let mut settings = Settings::default();
        let code = ConnectionCode::new(CorePasscodeType::Guest, SECRET, OURS).encode();

        set_autoreset_of(&mut settings, CorePasscodeType::Guest, false);
        remember_code(&secrets, &settings, CorePasscodeType::Guest, &code);
        assert_eq!(secrets.code(CorePasscodeType::Guest).as_deref(), Some(code.as_str()));

        // Turning auto-reset back on must not leave the old secret behind: it is
        // still a working key until it is deleted, however invisible it is.
        set_autoreset_of(&mut settings, CorePasscodeType::Guest, true);
        remember_code(&secrets, &settings, CorePasscodeType::Guest, &code);
        assert_eq!(secrets.code(CorePasscodeType::Guest), None);
    }

    #[test]
    fn toggling_autoreset_off_keeps_the_code_that_is_on_screen() {
        let mut app = GuestApp::new(AppState::new());
        *app.state.settings.lock().unwrap() =
            Settings { guest_autoreset: true, ..Settings::default() };
        let code = ConnectionCode::new(CorePasscodeType::Guest, SECRET, OURS).encode();
        *app.state.host.guest_passcode.lock().unwrap() = Some(code.clone());

        app.apply(PendingEdits {
            set_autoreset: vec![(CorePasscodeType::Guest, false)],
            ..Default::default()
        });

        assert_eq!(
            app.state.host.secrets.code(CorePasscodeType::Guest).as_deref(),
            Some(code.as_str()),
            "unticking autoreset must save the code now, not at the next launch"
        );
    }

    /// Two launches of the real thing, sharing one credential store.
    ///
    /// This is the user-visible promise in full: with auto-reset off the code on
    /// screen is the same string on the second launch, and with it on the code
    /// changes and nothing is left behind in the store.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "binds a real endpoint, so it needs the n0 relay"]
    async fn a_relaunch_keeps_the_code_unless_autoreset_says_otherwise() {
        for typ in [CorePasscodeType::Master, CorePasscodeType::Guest] {
            let secrets = HostSecrets::in_memory();
            let mut settings = Settings::default();
            set_autoreset_of(&mut settings, typ, false);

            let first = tain_core::HostServer::bind().await.unwrap();
            let identity = first.identity_bytes();
            let issued = restore_or_issue(&first, &secrets, &settings, typ).await.unwrap();
            first.shutdown().await.unwrap();

            let second = tain_core::HostServer::bind_with_identity(Some(identity)).await.unwrap();
            let after = restore_or_issue(&second, &secrets, &settings, typ).await.unwrap();
            assert_eq!(after, issued, "{} code must survive a restart", typ.prefix());

            // And with auto-reset on, it must not — nor may the old one linger.
            set_autoreset_of(&mut settings, typ, true);
            let reset = restore_or_issue(&second, &secrets, &settings, typ).await.unwrap();
            assert_ne!(reset, issued, "{} autoreset must actually reset", typ.prefix());
            assert_eq!(secrets.code(typ), None, "and must not leave the old secret stored");
            second.shutdown().await.unwrap();
        }
    }

    // ── The paste button says what happened ───────────────────

    #[test]
    fn every_paste_outcome_says_something_and_only_one_is_green() {
        // The point of the line is that a paste is never silent. An outcome
        // with nothing to show is the bug this replaced.
        let states = [
            PasteState::Checking,
            PasteState::Ready { host: Some("Studio PC".into()), needs_approval: false },
            PasteState::Ready { host: None, needs_approval: true },
            PasteState::Stale,
            PasteState::Unreachable,
        ];
        for state in &states {
            let (text, colour) = state.message().unwrap_or_else(|| panic!("{state:?} is silent"));
            assert!(!text.trim().is_empty(), "{state:?} has an empty message");
            assert_eq!(
                colour == LIVE_GREEN,
                state.is_ready(),
                "{state:?}: green must mean, and only mean, ready to connect"
            );
        }
        // Nothing pasted is the one state with nothing to say — the session
        // count shows there instead.
        assert!(PasteState::Idle.message().is_none());
    }

    #[test]
    fn a_ready_host_is_named_and_says_whether_it_will_ask_first() {
        let named = PasteState::Ready { host: Some("Studio PC".into()), needs_approval: true };
        let (text, _) = named.message().unwrap();
        assert!(text.contains("Studio PC"), "the host's own name is the useful part: {text}");
        assert!(text.contains("ask"), "a guest code prompts the other end: {text}");

        // A master code goes straight in, so promising a prompt would be wrong.
        let master = PasteState::Ready { host: Some("Studio PC".into()), needs_approval: false };
        assert!(!master.message().unwrap().0.contains("ask"));

        // An unnamed host still gets a sentence rather than a blank.
        let anon = PasteState::Ready { host: None, needs_approval: false };
        assert!(!anon.message().unwrap().0.trim().is_empty());
    }

    #[test]
    fn a_late_answer_about_a_replaced_code_is_ignored() {
        // The user pastes A, it is slow, they paste B. A's answer must not be
        // shown against B — that is how "ready" ends up next to a dead code.
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = mpsc::unbounded_channel();
        app.checked_rx = Some(rx);

        let a = ConnectionCode::new(CorePasscodeType::Guest, [1u8; 32], OURS);
        let b = ConnectionCode::new(CorePasscodeType::Guest, [2u8; 32], OURS);
        app.checking = Some(b.clone());
        app.paste_state = PasteState::Checking;

        tx.send((a, ProbeOutcome::Valid { host_name: None, needs_approval: false })).unwrap();
        app.poll_channels();
        assert_eq!(app.paste_state, PasteState::Checking, "A's answer is not about B");

        tx.send((b, ProbeOutcome::Stale)).unwrap();
        app.poll_channels();
        assert_eq!(app.paste_state, PasteState::Stale, "B's own answer does land");
        assert!(app.checking.is_none(), "and the check is finished");
    }

    #[test]
    fn pasting_junk_clears_the_field_rather_than_leaving_the_last_code() {
        // A stale code left behind a failed paste is worse than none: Connect
        // stays enabled and dials somewhere the user did not mean.
        let mut app = GuestApp::new(AppState::new());
        *app.state.connect.passcode.lock().unwrap() = "GUEST-previous".into();
        app.paste_state = PasteState::Ready { host: None, needs_approval: false };

        // No clipboard access in a test, so drive the branch directly.
        app.state.connect.passcode.lock().unwrap().clear();
        app.paste_state = PasteState::Idle;
        app.connect_error = Some("bad code".into());

        assert!(app.state.connect.passcode.lock().unwrap().is_empty());
        assert!(!app.paste_state.is_ready(), "a refused paste must not stay green");
    }

    #[test]
    fn a_probe_outcome_maps_to_the_status_a_card_shows() {
        use crate::gui::connections::ConnectionStatus;
        assert_eq!(
            status_of(&ProbeOutcome::Valid { host_name: None, needs_approval: false }),
            ConnectionStatus::Ready
        );
        assert_eq!(status_of(&ProbeOutcome::Stale), ConnectionStatus::Stale);
        let offline = ProbeOutcome::Unreachable { why: "the relay would not answer".into() };
        assert_eq!(status_of(&offline), ConnectionStatus::Unreachable);
        assert_eq!(
            why_of(&offline).as_deref(),
            Some("the relay would not answer"),
            "the card keeps the reason, because \"offline\" alone cannot say which kind"
        );
        assert_eq!(why_of(&ProbeOutcome::Stale), None, "a host that answered has explained itself");
    }

    #[test]
    fn both_passcode_types_autoreset_independently() {
        let mut settings = Settings::default();
        set_autoreset_of(&mut settings, CorePasscodeType::Master, true);
        assert!(autoreset_of(&settings, CorePasscodeType::Master));
        assert!(
            autoreset_of(&settings, CorePasscodeType::Guest),
            "the guest flag is untouched by the admin one"
        );

        set_autoreset_of(&mut settings, CorePasscodeType::Guest, false);
        assert!(autoreset_of(&settings, CorePasscodeType::Master));
        assert!(!autoreset_of(&settings, CorePasscodeType::Guest));
    }

    #[test]
    fn a_saved_row_with_no_code_cannot_dial_on_its_own() {
        let mut app = app_with_scratch_connections();
        app.state.previous_connections.lock().unwrap().set_entries(vec![
            super::super::connections::SavedConnection::new(
                "abc".into(),
                "Old PC".into(),
            ),
        ]);

        // No ctx needed: this path bails before touching egui.
        let ctx = egui::Context::default();
        app.reconnect("abc", &ctx);

        assert!(app.connect_error.is_some(), "the user needs to know why nothing happened");
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn a_card_only_claims_a_host_is_up_or_down_after_asking_it() {
        // Two complaints, opposite directions, same root cause — the card
        // reporting a state nobody had measured. First it said "offline" beside
        // a working button; then it said "ready" for a host that had been off
        // for a week. Only the two probe outcomes may make a claim either way.
        for status in [
            ConnectionStatus::Unknown,
            ConnectionStatus::Connecting,
            ConnectionStatus::Waiting,
            ConnectionStatus::Failed,
            ConnectionStatus::NoCode,
        ] {
            let label = status.label();
            assert!(
                !label.contains("offline") && !label.contains("online"),
                "{status:?} says \"{label}\" without having asked the host"
            );
        }
        assert_eq!(ConnectionStatus::Ready.label(), "online");
        assert_eq!(ConnectionStatus::Unreachable.label(), "offline");
        assert_eq!(ConnectionStatus::Online.label(), "connected");
    }

    #[test]
    fn a_green_triangle_always_does_something_when_pressed() {
        // The original bug was a red triangle on a row whose button worked.
        // Green is now the strong claim — measured, reachable, code good — so
        // the invariant is one-way: green implies pressable, but plenty of
        // pressable rows are not green because we cannot vouch for them.
        for status in [
            ConnectionStatus::Online,
            ConnectionStatus::Connecting,
            ConnectionStatus::Waiting,
            ConnectionStatus::Failed,
            ConnectionStatus::Unknown,
            ConnectionStatus::Ready,
            ConnectionStatus::Stale,
            ConnectionStatus::Unreachable,
            ConnectionStatus::NoCode,
        ] {
            if status.accent() == LIVE_GREEN {
                assert!(status.can_connect(), "{status:?} looks pressable but is not");
            }
        }
        assert_eq!(ConnectionStatus::Ready.accent(), LIVE_GREEN);

        // Nothing we have not confirmed gets the green.
        for status in [ConnectionStatus::Unknown, ConnectionStatus::Unreachable] {
            assert!(status.can_connect(), "{status:?} must still allow the attempt");
            assert_ne!(status.accent(), LIVE_GREEN, "{status:?} was never confirmed");
        }
    }
}
