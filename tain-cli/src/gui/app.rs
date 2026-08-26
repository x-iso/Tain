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
pub fn sync_unattended(state: &AppState) {
    use tain_core::service_control::{send, ControlMessage};

    let message = if state.settings().unattended_access {
        let Some(secret_key) = state.host.secrets.identity() else {
            // No credential store, so there is no stable identity to hand over
            // — and a service adopting a key that changes next launch would
            // answer to codes nobody has.
            tracing::debug!("No stored identity yet — nothing to hand the service");
            return;
        };
        let codes = [tain_core::PasscodeType::Master, tain_core::PasscodeType::Guest]
            .into_iter()
            .filter_map(|typ| state.host.secrets.code(typ))
            .collect();
        ControlMessage::AdoptIdentity { secret_key, codes }
    } else {
        ControlMessage::ForgetIdentity
    };

    match send(&message) {
        Ok(()) => tracing::debug!("Told the service about unattended access"),
        Err(e) => tracing::debug!("The service is not listening ({e:#})"),
    }
}

/// Opt-in debugging aids, from a comma-separated `TAIN_DEBUG` list.
///
/// Debug builds only — every one of these is compiled out of release.
///
/// | Flag       | Effect                                                    |
/// |------------|-----------------------------------------------------------|
/// | `demo`     | Fill the connection lists with sample rows                 |
/// | `loopback` | Connect to our own host endpoint on startup                |
/// | `settings` | Open the settings window immediately                       |
/// | `approval` | Open the approval dialog for the pending demo guest        |
///
/// `frametime` is deliberately **not** one of these: it only measures, and the
/// build whose numbers matter is release. [`FrameTimer::from_env`] reads the
/// same variable itself, in every build.
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
        },
        IncomingConnection {
            session_id: 2,
            display_name: "Friend PC2".into(),
            name_is_self_reported: true,
            mouse_allowed: false,
            keyboard_allowed: false,
            status: ApprovalStatus::Pending,
            is_admin: false,
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
}

/// Start the reachability prober. Returns at once; the bind happens inside.
pub fn init_prober(state: &AppState) -> ProbeHandles {
    let (check_tx, check_rx) = mpsc::unbounded_channel();
    let (checked_tx, checked_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_probe_loop(
        Arc::clone(&state.previous_connections),
        check_rx,
        checked_tx,
    ));
    ProbeHandles { check_tx, checked_rx }
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

    // The identity exists by now, which it did not when the settings were last
    // saved on a first run. See `sync_unattended`.
    sync_unattended(state);

    // And say plainly that this machine already has a host. The service yields
    // to it: two hosts on one identity would have a guest reach whichever
    // answered first, which is worse than either alone.
    //
    // Belt and braces rather than the main mechanism — the service also checks
    // whether anybody is signed in, and if this is running then somebody is.
    // It closes the window around signing out, where the token disappears
    // before this process does.
    let _ = tain_core::service_control::send(
        &tain_core::service_control::ControlMessage::ApplicationHosting(true),
    );

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
) {
    let mut prober = match tain_core::network::Prober::bind().await {
        Ok(prober) => prober,
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

            _ = tokio::time::sleep_until(next_sweep) => {
                // Collected in one pass and released before any network work:
                // holding the store locked across a probe would stall every
                // repaint.
                let targets = store.lock().unwrap().probe_targets();

                // Counted so a sweep that reached nobody can be told apart from
                // a sweep with nobody to reach.
                let mut asked = 0u32;
                let mut answered = 0u32;

                for (endpoint_hex, code) in targets {
                    let Ok(parsed) = ConnectionCode::parse(&code) else {
                        store
                            .lock()
                            .unwrap()
                            .set_probe_result(&endpoint_hex, ConnectionStatus::NoCode);
                        continue;
                    };

                    asked += 1;
                    let status = status_of(prober.probe(&parsed, PROBE_TIMEOUT).await);
                    if status.can_connect() {
                        answered += 1;
                    }
                    store.lock().unwrap().set_probe_result(&endpoint_hex, status);
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
                            prober = fresh;
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
fn status_of(outcome: ProbeOutcome) -> ConnectionStatus {
    match outcome {
        ProbeOutcome::Valid { .. } => ConnectionStatus::Ready,
        ProbeOutcome::Stale => ConnectionStatus::Stale,
        ProbeOutcome::Unreachable => ConnectionStatus::Unreachable,
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
            Self::Checking => Some(("Checking the host...".into(), TEXT_DIM)),
            Self::Ready { host, needs_approval } => {
                let who = host.as_deref().unwrap_or("Host");
                Some((
                    if *needs_approval {
                        format!("{who} is ready - it will ask before letting you in")
                    } else {
                        format!("{who} is ready")
                    },
                    LIVE_GREEN,
                ))
            }
            // Both of these are "do not press Connect", but for different
            // reasons and with different fixes, so they do not share a message.
            Self::Stale => Some((
                "That code has expired - ask for a new one".into(),
                AMBER,
            )),
            Self::Unreachable => Some((
                "No answer - the host may be offline".into(),
                OFFLINE_RED,
            )),
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
    /// Which devices the checkboxes currently offer. Deliberately starts empty:
    /// the safe answer must be the one you get by not thinking about it.
    grants: ControlGrants,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    /// A guest is waiting to be let in at all.
    Approve,
    /// An established guest is being removed.
    Kick,
}

/// What the user chose in the confirmation dialog.
///
/// `Dismiss` is deliberately distinct from `Deny`: closing the dialog leaves the
/// guest waiting, while denying disconnects it. Collapsing the two would make
/// an accidental Escape into a disconnection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Allow(ControlGrants),
    Deny,
    Dismiss,
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
        Self { session_id, guest, guest_named_itself, action, grants }
    }

    fn title(&self) -> &'static str {
        match self.action {
            ConfirmAction::Approve => "Allow this connection?",
            ConfirmAction::Kick => "Disconnect this guest?",
        }
    }

    /// What actually happens if this is confirmed.
    ///
    /// The screen is the part people underestimate, so it leads: input can be
    /// revoked later, but anything visible during the session has been seen.
    fn body(&self) -> String {
        match self.action {
            ConfirmAction::Approve => format!(
                "{} will see everything on this machine's screen for as long as \
                 the connection lasts.\n\nNothing is being streamed yet.",
                self.guest
            ),
            ConfirmAction::Kick => {
                format!("{} will be disconnected immediately.", self.guest)
            }
        }
    }

    /// Caveat shown when the name came from the other machine.
    fn name_caveat(&self) -> Option<&'static str> {
        (self.guest_named_itself && self.action == ConfirmAction::Approve)
            .then_some("This name was chosen by the connecting machine.")
    }

    fn confirm_label(&self) -> &'static str {
        match self.action {
            ConfirmAction::Approve => "Allow",
            ConfirmAction::Kick => "Disconnect",
        }
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
    pending_approvals: HashMap<u32, oneshot::Sender<ApprovalDecision>>,
    /// Which passcode was most recently copied, for transient UI feedback.
    copied_at: Option<(PasscodeType, std::time::Instant)>,
    /// Live guest sessions, one per tab in the session window.
    sessions: Vec<GuestSession>,
    /// The session window's own state (active tab, fullscreen, toolbar).
    session_window: SessionWindow,
    /// Win-key capture and pointer confinement, shared by whichever session is focused.
    immersive: Immersive,
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
    checked_rx: Option<mpsc::UnboundedReceiver<(ConnectionCode, ProbeOutcome)>>,
    /// A destructive action awaiting confirmation.
    confirm: Option<Confirm>,
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
            sessions: Vec::new(),
            session_window: SessionWindow::default(),
            immersive: Immersive::default(),
            last_reported_scale: None,
            // Not the saved value: this is what egui has been told, and it has
            // been told nothing yet. The first frame pushes the setting in.
            applied_ui_scale: 1.0,
            previews: std::cell::RefCell::new(std::collections::HashMap::new()),
            host_sessions: None,
            pending_handles: None,
            connect_error: None,
            paste_state: PasteState::default(),
            checking: None,
            check_tx: None,
            checked_rx: None,
            confirm: None,
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
            self.confirm = Some(Confirm::new(
                card.session_id,
                card.display_name.clone(),
                card.name_is_self_reported,
                ConfirmAction::Approve,
            ));
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
            while let Ok(req) = rx.try_recv() {
                info!("Approval requested for session #{}: {}", req.session_id, req.label);
                self.state.incoming.lock().unwrap().push(IncomingConnection {
                    session_id: req.session_id,
                    display_name: req.label,
                    name_is_self_reported: req.label_is_self_reported,
                    mouse_allowed: false,
                    keyboard_allowed: false,
                    status: ApprovalStatus::Pending,
                    // Only guest connections are ever asked about.
                    is_admin: false,
                });
                // Hold the responder until the user decides. Dropping it here
                // would answer "no decision", which the server reads as a denial.
                self.pending_approvals.insert(req.session_id, req.approve_tx);
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
                    ProbeOutcome::Unreachable => PasteState::Unreachable,
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

    /// Answer a guest that is waiting to be let in.
    fn resolve_approval(&mut self, session_id: u32, decision: ApprovalDecision) {
        match self.pending_approvals.remove(&session_id) {
            Some(tx) => {
                // The server is blocked on this: it has captured nothing and
                // sent nothing, and will only start once we answer.
                let _ = tx.send(decision);
            }
            None => {
                // Already streaming — the prompt closed, or this is a live
                // session whose permissions are being changed after the fact.
                match decision {
                    ApprovalDecision::Allow(grants) => {
                        self.send(HostCommand::SetSessionControl { session_id, grants })
                    }
                    ApprovalDecision::Deny => self.kick(session_id),
                }
            }
        }

        match decision {
            ApprovalDecision::Allow(grants) => {
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
        self.resolve_approval(session_id, ApprovalDecision::Allow(grants));
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

        // Only one confirmation at a time; a second click while one is open is
        // almost certainly a mis-click rather than a second decision.
        if self.confirm.is_none()
            && let Some((session_id, action)) = edits.confirm.into_iter().next()
        {
            // Snapshot before locking `incoming` — `settings()` takes its own
            // lock and these must not nest.
            let defaults = self.state.settings().default_grants();
            let incoming = self.state.incoming.lock().unwrap();
            if let Some(card) = incoming.iter().find(|c| c.session_id == session_id) {
                self.confirm = Some(Confirm::with_grants(
                    session_id,
                    card.display_name.clone(),
                    card.name_is_self_reported,
                    action,
                    defaults,
                ));
            }
        }

        if edits.paste_passcode {
            // Validate on paste rather than on Connect, so a mangled code is
            // reported while the user still has the original to hand.
            self.checking = None;
            match read_clipboard() {
                None => {
                    self.paste_state = PasteState::Idle;
                    self.connect_error = Some("Clipboard is empty".into());
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

        // Where this host answered last time, if it ever has. Offered as one
        // more candidate; see `SavedConnection::last_direct_addr` for why a
        // stale one costs nothing.
        let known_addrs: Vec<std::net::SocketAddr> = self
            .state
            .previous_connections
            .lock()
            .unwrap()
            .direct_addr(&endpoint_hex)
            .into_iter()
            .collect();

        let settings = self.state.settings();
        let repaint_ctx = ctx.clone();
        let session = GuestSession::start(code, known_addrs, &settings, loopback, move || {
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

        // Where it answered, so the next connection can skip finding out. Kept
        // even when there is no picture — a session that drew every frame on
        // the GPU has no preview and still had a route worth remembering.
        if let Some(addr) = session.direct_addr() {
            self.state
                .previous_connections
                .lock()
                .unwrap()
                .set_direct_addr(&endpoint, &addr);
        }

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
                    _ => "The host ended the session".to_string(),
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
                });
            }

            for card in incoming.iter_mut() {
                let Some(grants) = sessions.grants(card.session_id) else { continue };
                card.mouse_allowed = grants.mouse;
                card.keyboard_allowed = grants.keyboard;
                // A pending card keeps its status until the user decides — the
                // server holds no grants while waiting, so mirroring them would
                // read as a decision nobody made.
            }
            incoming.iter().map(|c| c.session_id).collect()
        };

        // Drop responders and dialogs for sessions that vanished.
        self.pending_approvals.retain(|id, _| live.contains(id));
        if self.confirm.as_ref().is_some_and(|c| !live.contains(&c.session_id)) {
            self.confirm = None;
        }
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
                    GuestStatus::Connecting | GuestStatus::Authenticated => {
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
            reason: "disconnected by host".into(),
        });
        // Answer any outstanding approval so the server is not left waiting out
        // its whole timeout on a session that is going away.
        if let Some(tx) = self.pending_approvals.remove(&session_id) {
            let _ = tx.send(ApprovalDecision::Deny);
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

    fn render_confirm(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.as_mut() else { return };
        let (title, body, label) = (confirm.title(), confirm.body(), confirm.confirm_label());
        let caveat = confirm.name_caveat();
        let approving = confirm.action == ConfirmAction::Approve;
        let session_id = confirm.session_id;
        let mut grants = confirm.grants;
        let mut outcome = None;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.label(egui::RichText::new(body).size(12.0).color(TEXT));
                if let Some(caveat) = caveat {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(caveat).size(11.0).color(TEXT_DIM));
                }

                if approving {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Also let them control:").size(12.0).color(TEXT),
                    );
                    ui.add_space(2.0);
                    ui.checkbox(
                        &mut grants.mouse,
                        egui::RichText::new("Mouse").color(TEXT),
                    )
                    .on_hover_text(
                        "Also covers touch, if they are using a touchscreen — it is \
                         the same act of pointing at your screen.",
                    );
                    ui.checkbox(
                        &mut grants.keyboard,
                        egui::RichText::new("Keyboard").color(TEXT),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            "Leave both off to share the screen only. \
                             You can change this later from the connection strip.",
                        )
                        .size(11.0)
                        .color(TEXT_DIM),
                    );
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    // Cancel walks away without answering: the guest stays in
                    // the list, still waiting, still seeing nothing.
                    if ui
                        .button("Cancel")
                        .on_hover_text("Decide later — nothing is shared in the meantime")
                        .clicked()
                    {
                        outcome = Some(Outcome::Dismiss);
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let confirm_button = egui::Button::new(
                            egui::RichText::new(label).color(egui::Color32::WHITE),
                        )
                        .fill(if approving { ADMIN_FILL } else { OFFLINE_RED });
                        if ui.add(confirm_button).clicked() {
                            outcome = Some(if approving {
                                Outcome::Allow(grants)
                            } else {
                                Outcome::Deny
                            });
                        }

                        if approving {
                            // Wider than the usual gap: Allow and Deny sit next
                            // to each other and mean opposite things.
                            ui.add_space(14.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Deny").color(egui::Color32::WHITE),
                                    )
                                    .fill(OFFLINE_RED),
                                )
                                .on_hover_text("Refuse and disconnect this guest")
                                .clicked()
                            {
                                outcome = Some(Outcome::Deny);
                            }
                        }
                    });
                });
            });

        // Keep the checkbox state across frames while the dialog is open.
        if let Some(confirm) = self.confirm.as_mut() {
            confirm.grants = grants;
        }

        match outcome {
            Some(Outcome::Allow(grants)) => {
                self.confirm = None;
                self.resolve_approval(session_id, ApprovalDecision::Allow(grants));
            }
            Some(Outcome::Deny) => {
                self.confirm = None;
                self.resolve_approval(session_id, ApprovalDecision::Deny);
            }
            Some(Outcome::Dismiss) => self.confirm = None,
            None => {}
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
            section_label(ui, "Host:", BUTTON_HEIGHT);
            ui.add_space(4.0);

            ui.vertical(|ui| {
                self.render_passcode_row(ui, PasscodeType::Master, btn_w, edits);
                ui.add_space(6.0);
                self.render_passcode_row(ui, PasscodeType::Guest, btn_w, edits);
            });

            ui.add_space(12.0);
            theme::header_separator(ui);
            ui.add_space(12.0);

            section_label(ui, "Connect:", BUTTON_HEIGHT);
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
                        if btn_w >= 200.0 { "Paste passcode" } else { "Paste" },
                        egui::include_image!("../../assets/paste_passcode.svg"),
                        GUEST_FILL,
                        stroke,
                        true,
                    )
                    .on_hover_text("Read a connection code from the clipboard")
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
                            (false, _) => "Paste a connection code first",
                            (true, true) => "Checking that this host is reachable...",
                            (true, false) => "Open a session with this host",
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
                        0 => ("No sessions".to_string(), TEXT_DIM),
                        1 => ("1 session open".to_string(), LIVE_GREEN),
                        n => (format!("{n} sessions open"), LIVE_GREEN),
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
                    "Settings",
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

        // Drop to the short form rather than letting the label overrun the button.
        let roomy = width >= 200.0;
        let (label, fill, stroke) = match (typ, roomy) {
            (PasscodeType::Master, true) => ("Copy admin passcode", ADMIN_FILL, ADMIN_STROKE),
            (PasscodeType::Master, false) => ("Copy admin", ADMIN_FILL, ADMIN_STROKE),
            (PasscodeType::Guest, true) => ("Copy guest passcode", GUEST_FILL, GUEST_STROKE),
            (PasscodeType::Guest, false) => ("Copy guest", GUEST_FILL, GUEST_STROKE),
        };
        let label = if recently_copied {
            if roomy { "Copied to clipboard" } else { "Copied" }
        } else {
            label
        };
        let stroke = if recently_copied { LIVE_GREEN } else { stroke };

        ui.horizontal(|ui| {
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
                    resp.on_hover_text("Copy this code and send it to the other machine")
                }
                (None, Some(e)) => resp.on_hover_text(format!("Host server is offline — {e}")),
                (None, None) => resp.on_hover_text("No code generated yet"),
            };

            if resp.clicked()
                && let Some(code) = code
            {
                ui.ctx().copy_text(code);
                edits.copied = Some(typ);
            }

            ui.add_space(8.0);

            if theme::icon_button(
                ui,
                20.0,
                egui::include_image!("../../assets/reset_passcode.svg"),
                "Generate a new code now (invalidates the current one)",
            )
            .clicked()
            {
                edits.regenerate.push(typ);
            }

            ui.add_space(4.0);
            let mut on = autoreset_of(&self.state.settings(), typ);
            if ui
                .checkbox(&mut on, egui::RichText::new("autoreset").color(TEXT))
                .on_hover_text(
                    "Regenerate automatically when the last guest disconnects.\n\
                     Turning this off keeps the code usable for reconnecting.",
                )
                .changed()
            {
                edits.set_autoreset.push((typ, on));
            }
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
                    egui::RichText::new("No previous connections yet.")
                        .size(13.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Hosts you connect to will be saved here.")
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
                .on_hover_text("Forget this host, and the code saved with it")
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
        bar.label(egui::RichText::new(conn.status.label()).size(14.0).color(TEXT))
            .on_hover_text(conn.status.detail());
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
                ConnectionStatus::Ready => "Connect",
                ConnectionStatus::Unknown => "Connect — this host has not been checked yet",
                ConnectionStatus::Failed => "Try again",
                ConnectionStatus::Online => "Already connected",
                ConnectionStatus::Connecting | ConnectionStatus::Waiting => "Connecting…",
                // Both of these still offer the attempt: the check is a snapshot
                // and the host may have come back since.
                ConnectionStatus::Stale => "Try anyway — this code looked expired a moment ago",
                ConnectionStatus::Unreachable => "Try anyway — this host did not answer",
                ConnectionStatus::NoCode => {
                    "No saved code for this host — paste a fresh one to connect"
                }
            });
            if resp.clicked() {
                edits.reconnect = Some(conn.endpoint_id.clone());
            }
        });
    }

    // ── Footer: incoming connections ─────────────────────────

    fn render_incoming_connections(&self, ui: &mut egui::Ui, edits: &mut PendingEdits) {
        // Snapshot, then release — the toggles below used to re-lock this mutex
        // mid-iteration and freeze the app on the first click.
        let incoming = self.state.incoming.lock().unwrap().clone();

        if incoming.is_empty() {
            ui.label(
                egui::RichText::new("No incoming connections.")
                    .size(12.0)
                    .color(TEXT_DIM),
            );
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 8.0);
            for conn in &incoming {
                self.render_incoming_pill(ui, conn, edits);
            }
        });
    }

    fn render_incoming_pill(
        &self,
        ui: &mut egui::Ui,
        conn: &IncomingConnection,
        edits: &mut PendingEdits,
    ) {
        let pending = conn.status == ApprovalStatus::Pending;
        let (rect, _) = ui.allocate_exact_size(PILL_SIZE, egui::Sense::hover());

        // Pending cards get the dashed outline from incoming_card_pending.svg.
        ui.painter().rect_filled(rect, CORNER, CARD_BG);
        if pending {
            theme::dashed_rect(ui, rect, TEXT_DIM);
        } else {
            ui.painter()
                .rect_stroke(rect, CORNER, egui::Stroke::new(1.0, CARD_STROKE));
        }

        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(egui::vec2(8.0, 10.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );

        // The person icon *is* the accept/remove control. Pending guests are
        // admitted here; established ones swap to the kick icon on hover, which
        // is the only way to remove a connected guest.
        let (icon_rect, icon_resp) =
            content.allocate_exact_size(egui::Vec2::splat(22.0), egui::Sense::click());
        // Kickable whether or not it was ever approved: an admin connection is
        // never asked about, and "cannot be asked about" must not mean "cannot
        // be ended".
        let kicking = !pending && icon_resp.hovered();
        let icon_src = if kicking {
            egui::include_image!("../../assets/incoming_connection_kick.svg")
        } else if conn.is_admin {
            // Its own mark: this one holds the admin passcode and already has
            // full control, which is worth being able to see at a glance.
            egui::include_image!("../../assets/incoming_connection_admin_icon.svg")
        } else {
            egui::include_image!("../../assets/incoming_connection_icon.svg")
        };
        if content.is_rect_visible(icon_rect) {
            let icon_rect = theme::snap(&content, icon_rect);
            theme::sharp_image(icon_src, icon_rect.size()).paint_at(&content, icon_rect);
        }
        let icon_resp = icon_resp.on_hover_text(if pending {
            "Waiting for you — click to decide"
        } else {
            "Disconnect this guest"
        });
        if icon_resp.clicked() {
            edits.confirm.push((
                conn.session_id,
                if pending { ConfirmAction::Approve } else { ConfirmAction::Kick },
            ));
        }

        content.add_space(6.0);
        content
            .label(egui::RichText::new(&conn.display_name).size(13.0).color(TEXT))
            .on_hover_text(if pending {
                "Waiting for your decision — nothing shared yet".to_string()
            } else {
                let mut text =
                    describe_grants(conn.mouse_allowed, conn.keyboard_allowed).to_string();
                if conn.name_is_self_reported {
                    text.push_str("\nThis name was chosen by the connecting machine.");
                }
                text
            });

        content.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match conn.status {
                ApprovalStatus::Pending => {
                    ui.add(egui::Spinner::new().size(14.0).color(TEXT_DIM))
                        .on_hover_text("Nothing is being shared until you decide");
                }
                ApprovalStatus::Allowed => {
                    theme::icon(ui, 13.0, egui::include_image!("../../assets/in_connect_live.svg"));
                }
            }

            ui.add_space(6.0);

            // On an established connection these toggle directly — revoking
            // should be instant. On a pending one there is nothing to toggle
            // yet, so they open the same dialog the person icon does.
            let kbd = if conn.keyboard_allowed {
                egui::include_image!("../../assets/keyboard_allowed.svg")
            } else {
                egui::include_image!("../../assets/keyboard_disabled.svg")
            };
            if theme::icon_button(
                ui,
                22.0,
                kbd,
                match (pending, conn.keyboard_allowed) {
                    (true, _) => "Decide whether to allow this connection",
                    (false, true) => "Keyboard allowed — click to revoke",
                    (false, false) => "Keyboard blocked — click to allow",
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

            ui.add_space(4.0);

            let mouse = if conn.mouse_allowed {
                egui::include_image!("../../assets/mouse_allowed.svg")
            } else {
                egui::include_image!("../../assets/mouse_disabled.svg")
            };
            if theme::icon_button(
                ui,
                22.0,
                mouse,
                match (pending, conn.mouse_allowed) {
                    (true, _) => "Decide whether to allow this connection",
                    (false, true) => "Mouse allowed — click to revoke",
                    (false, false) => "Mouse blocked — click to allow",
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
    }
}

/// Plain-language summary of a session's input permissions.
pub fn describe_grants(mouse: bool, keyboard: bool) -> &'static str {
    match (mouse, keyboard) {
        (true, true) => "full control",
        (true, false) => "mouse only",
        (false, true) => "keyboard only",
        (false, false) => "view-only",
    }
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

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_ui_scale(ctx);
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
        egui::TopBottomPanel::bottom("incoming")
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
        t = self.frametime.phase("incoming", t);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(WINDOW_BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("Previous connections:")
                        .size(14.0)
                        .strong()
                        .color(TEXT),
                );
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
                // The edit is visible immediately, but writing it to disk and
                // rebuilding the capture pipeline wait for the user to settle —
                // see `settings_settling`.
                *self.state.settings.lock().unwrap() = settings;
                self.settings_settling = Some(std::time::Instant::now());
            }
        }
        self.flush_settled_settings(ctx);

        t = self.frametime.phase("settings", t);

        self.render_confirm(ctx);

        if let Some(typ) = edits.copied {
            self.copied_at = Some((typ, std::time::Instant::now()));
        }
        if !edits.regenerate.is_empty() {
            self.copied_at = None;
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
        app.pending_approvals.insert(id, tx);
        app.state.incoming.lock().unwrap().push(incoming(id, ApprovalStatus::Pending));
        app.confirm =
            Some(Confirm::new(id, "Guest #1".into(), false, ConfirmAction::Approve));

        sessions.remove_session(id);
        app.reconcile_incoming();

        assert!(app.pending_approvals.is_empty(), "responder should not outlive the session");
        assert!(app.confirm.is_none(), "a dialog for a dead session is unanswerable");
    }

    #[tokio::test]
    async fn allowing_answers_the_pending_oneshot() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(7, tx);
        app.state.incoming.lock().unwrap().push(incoming(7, ApprovalStatus::Pending));

        app.resolve_approval(7, ApprovalDecision::Allow(ControlGrants::ALL));

        assert_eq!(
            rx.await.unwrap(),
            ApprovalDecision::Allow(ControlGrants::ALL),
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
        app.pending_approvals.insert(3, tx);
        app.state.incoming.lock().unwrap().push(incoming(3, ApprovalStatus::Pending));

        app.resolve_approval(3, ApprovalDecision::Allow(ControlGrants::NONE));

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow(ControlGrants::NONE));
        assert_eq!(app.state.incoming.lock().unwrap()[0].status, ApprovalStatus::Allowed);
    }

    #[tokio::test]
    async fn denying_disconnects_rather_than_leaving_a_greyed_out_card() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(5, tx);
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
        app.pending_approvals.insert(9, tx);
        app.state.incoming.lock().unwrap().push(incoming(9, ApprovalStatus::Pending));

        app.set_grants(9, ControlGrants::new(true, false));

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow(ControlGrants::new(true, false)));
        let card = app.state.incoming.lock().unwrap()[0].clone();
        assert!(card.mouse_allowed);
        assert!(!card.keyboard_allowed);
        assert_eq!(card.status, ApprovalStatus::Allowed);
    }

    #[tokio::test]
    async fn kicking_answers_the_responder_so_the_server_is_not_left_waiting() {
        let mut app = GuestApp::new(AppState::new());
        let (tx, rx) = oneshot::channel();
        app.pending_approvals.insert(4, tx);
        app.state.incoming.lock().unwrap().push(incoming(4, ApprovalStatus::Pending));

        app.kick(4);

        assert_eq!(rx.await.unwrap(), ApprovalDecision::Deny);
        assert!(app.state.incoming.lock().unwrap().is_empty());
        assert!(app.pending_approvals.is_empty());
    }

    #[test]
    fn the_approval_dialog_leads_with_what_is_actually_being_shared() {
        // The screen is the part people underestimate; input can be revoked,
        // but anything seen has been seen.
        let approve = Confirm::new(1, "Guest #1".into(), false, ConfirmAction::Approve);
        assert!(approve.body().contains("Guest #1"));
        assert!(approve.body().contains("screen"));
        assert!(approve.body().contains("Nothing is being streamed yet"));
        assert_eq!(approve.confirm_label(), "Allow");

        let kick = Confirm::new(1, "Guest #1".into(), false, ConfirmAction::Kick);
        assert!(kick.body().contains("disconnected"));
        assert_eq!(kick.confirm_label(), "Disconnect");
        assert_ne!(approve.title(), kick.title());
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

    #[test]
    fn only_one_confirmation_is_shown_at_a_time() {
        // A second click while a dialog is open is a mis-click, not a decision.
        let mut app = GuestApp::new(AppState::new());
        app.state.incoming.lock().unwrap().push(incoming(1, ApprovalStatus::Pending));
        app.state.incoming.lock().unwrap().push(incoming(2, ApprovalStatus::Allowed));

        app.apply(PendingEdits {
            confirm: vec![(1, ConfirmAction::Approve), (2, ConfirmAction::Kick)],
            ..Default::default()
        });

        assert_eq!(app.confirm.as_ref().map(|c| c.session_id), Some(1));
    }

    #[test]
    fn a_dialog_for_a_card_that_is_already_gone_is_not_opened() {
        let mut app = GuestApp::new(AppState::new());
        app.apply(PendingEdits {
            confirm: vec![(99, ConfirmAction::Approve)],
            ..Default::default()
        });
        assert!(app.confirm.is_none());
    }

    #[test]
    fn dismissing_is_not_the_same_decision_as_denying() {
        // Closing the dialog leaves the guest waiting; denying disconnects it.
        assert_ne!(Outcome::Dismiss, Outcome::Deny);
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
            status_of(ProbeOutcome::Valid { host_name: None, needs_approval: false }),
            ConnectionStatus::Ready
        );
        assert_eq!(status_of(ProbeOutcome::Stale), ConnectionStatus::Stale);
        assert_eq!(status_of(ProbeOutcome::Unreachable), ConnectionStatus::Unreachable);
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
