//! iroh-based P2P networking layer.
//!
//! Provides `HostServer` (accepts incoming connections, manages sessions) and
//! `GuestClient` (connects to a host via passcode-protected ticket).
//!
//! The ALPN protocol name is `"tain/6"` — every connection carrying this ALPN
//! is routed through our handler. iroh handles NAT traversal automatically:
//! direct QUIC connections when possible, relay fallback otherwise.
//!
//! ## Video has a stream to itself
//!
//! It was `tain/1` while everything shared one bidirectional stream: frames,
//! input, cursor updates, grants, configuration. QUIC guarantees ordering
//! *within* a stream, so a keyframe part-way out delayed everything queued
//! behind it — and what queues behind a keyframe is the pointer, which is the
//! one thing somebody watches continuously.
//!
//! So frames now go down a unidirectional stream of their own, and everything
//! else keeps the bidirectional one. Head-of-line blocking between the two is
//! gone; within video it remains, which is correct, because frames of an
//! inter-frame codec have to arrive in order anyway.
//!
//! The version went up rather than negotiating both shapes. Nothing is
//! deployed that this has to interoperate with, and a compatibility path for a
//! protocol nobody is running would be code written to be deleted.
//!
//! ## Stream direction
//!
//! The **guest** calls `open_bi()`; the **host** calls `accept_bi()`. Both sides
//! calling the same one deadlocks — nobody opens a stream and both wait forever.
//!
//! ## Session loop shape
//!
//! Each session runs a reader task and a writer loop over the two halves of one
//! bidirectional stream. They must not be merged into a single `select!` over
//! `read_stream_message`: that function is **not cancel-safe** (it can be dropped
//! between reading the 4-byte length prefix and the body, desynchronising the
//! stream). The reader owns `recv` exclusively and forwards replies to the writer
//! over a channel; the writer races that channel against the frame channel, both
//! of which *are* cancel-safe.
//!
//! ## Capture and encode run on their own thread
//!
//! Capturing and encoding a frame is **synchronous, CPU-bound work** — measured
//! at roughly 95 ms for one 2560x1600 frame. Running it inline on a tokio task,
//! which is what this used to do, blocks that worker thread for the duration.
//! The effects are not subtle and do not look like what they are: iroh's own
//! networking and this session's input reader are starved, so the pointer lags
//! seconds behind, frames trickle out, and the bandwidth graph looks *idle*
//! because nothing is being produced to send. It reads as a network problem and
//! is not one.
//!
//! So the pipeline lives on a dedicated OS thread and hands finished frames over
//! a **bounded** channel. Bounded is the important part: it applies backpressure,
//! so a slow link stops the encoder rather than queueing frames that are already
//! stale by the time they are sent. Frames are never dropped — with an
//! inter-frame codec, dropping one corrupts everything until the next keyframe.

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::*;

pub use super::encoder::{EncodeConfig, EncoderBackend, RateControl, VideoCodec};
pub use super::passcode::{PasscodeStore, PasscodeType};
pub use super::protocol::{ControlGrants, ControlStateMessage, Message};
pub use super::screen_capture::{CaptureConfig, QualityScale, ScreenFrame};
pub use super::session::SessionManager;

use super::passcode::redact;
pub use super::connection_code::ConnectionCode;

/// Application-layer protocol name used in iroh ALPN negotiation.
///
/// # Bump this whenever the wire format changes incompatibly
///
/// `Message` is bincode, which is positional and has no version field: a peer
/// handed a struct with a field it does not know about does not report a
/// mismatch, it misreads the bytes or fails to deserialise somewhere further
/// on. What that looks like from the outside is a connection that establishes
/// and then dies for no stated reason.
///
/// The ALPN is the one place two peers agree on anything before that can
/// happen. An unrecognised one is refused during the TLS handshake, with both
/// ends saying so, which is a far better failure than a mystery — so the cost of
/// bumping it is that mismatched builds cannot talk, which is exactly the
/// outcome wanted.
///
/// `tain/3` added the host's display list to `SessionStart`; `tain/4` made
/// choosing a display a `DisplayChoice` rather than a bare id, so "all of
/// them" could be asked for; `tain/5` added whether a display is virtual;
/// `tain/6` added the monitor's own name.
pub const ALPN: &[u8] = b"tain/6";

/// How long a starting host waits to reach a relay before listening anyway.
///
/// Long enough that an ordinary connection is made before the warning appears,
/// short enough that a censored link does not turn "starting the host" into a
/// hang with nothing on screen.
const HOST_ONLINE_WAIT: Duration = Duration::from_secs(10);

/// What the host decided about a guest that asked to connect.
///
/// This gates the *stream*, not just input: a guest sees nothing at all until
/// the host allows it. Streaming first and asking afterwards would mean a code
/// leaking once is enough to watch the screen forever.
/// Not `Copy`: the display grant is a list, and a list of screens is worth
/// more than the convenience of copying a decision around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Stream the desktop. `grants` says which input devices may be driven —
    /// [`ControlGrants::NONE`] is a legitimate answer, meaning view-only — and
    /// `displays` which screens may be seen at all.
    Allow {
        grants: ControlGrants,
        displays: super::protocol::DisplayGrant,
    },
    /// Refuse: the guest is disconnected without ever seeing a frame.
    Deny,
}

impl ApprovalDecision {
    /// Grants implied by this decision.
    pub fn grants(&self) -> ControlGrants {
        match self {
            Self::Allow { grants, .. } => *grants,
            Self::Deny => ControlGrants::NONE,
        }
    }
}

/// Approval request sent from host server to GUI when a guest passcode connects.
pub struct ApprovalRequest {
    /// Host-assigned session identifier for this connection.
    pub session_id: u32,
    /// Human-friendly label — the guest's own name if it published one,
    /// otherwise a generated "Guest #N".
    pub label: String,
    /// Whether `label` came from the guest rather than being generated here.
    /// A remote-supplied name should not be presented as though the host
    /// vouched for it.
    pub label_is_self_reported: bool,
    /// Whether this connection presented the **admin** passcode.
    ///
    /// A different question is being asked in each case, and the interface has
    /// to ask the right one. A guest is asking to watch, and the host chooses
    /// what to grant. An admin already has everything the moment it is let in —
    /// every screen, mouse and keyboard, and the lock screen too where the
    /// service is installed — so the only honest question is whether this is
    /// the owner, and saying yes remembers the answer.
    pub is_admin: bool,
    /// The guest's endpoint id: the public key its connection is authenticated
    /// with.
    ///
    /// What gets remembered when an admin connection is approved, and the
    /// reason the memory is worth anything — see [`SessionConfig::trusted_admins`].
    /// Nobody can present somebody else's, which is exactly what a name or an
    /// address cannot promise.
    pub endpoint_id: [u8; 32],
    /// Oneshot sender carrying the host's decision.
    pub approve_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// Whether somebody at the host has to say yes before this connection runs.
///
/// # The two rules, and why they are different
///
/// **Every guest, every time.** What a guest is allowed — which screens, mouse,
/// keyboard — is decided per connection, so there is nothing to remember and
/// nothing that could be skipped.
///
/// **An admin, the first time from each endpoint.** The admin passcode is
/// unrestricted access, so the useful question is not "what may this connection
/// do" but "is this the owner". Asked once per endpoint rather than once per
/// connection: asking every time would train the answer out of anyone who uses
/// their own machine, and an approval nobody reads protects nothing.
///
/// Pure, and separate from the connection it is asked about, because it is the
/// whole of the authorisation decision and every case of it should be readable
/// without a network.
/// How long everything that can say where the host is *now* gets before the
/// address it answered on last time joins the dial.
///
/// # Why this is a clock and not "when nothing was found"
///
/// It was the latter, and that turned out to mean the opposite of what it said.
/// On a filtered link mDNS and DNS do not hang — they complete, with nothing,
/// in a second or two — so `connect` returned `NoAddress` almost at once and
/// the saved address went in at about two seconds. The mainline DHT needs
/// eight, and often a second ask. So the lookup that still works where the
/// others are blocked never got a turn, and the fallback written to be a last
/// resort was in practice tried before the thing it was meant to come after.
///
/// Being gated on the two lookups that fail *fastest* is what made it first. A
/// clock is gated on the slowest one, which is the one that matters.
///
/// Public because the ordering is the property, and a property that has been
/// got wrong twice deserves a test that can name the boundary it must not
/// cross. See `a_host_that_can_be_reached_directly_is_carried_directly`.
pub const DISCOVERY_GRACE: Duration = Duration::from_secs(15);

/// This machine's identity for connections it makes to other machines.
///
/// # Why outgoing connections need a name at all
///
/// An endpoint id is a public key, and iroh mints a fresh one for any endpoint
/// that is not given a key — which is fine for a dial that only has to reach
/// somebody, and wrong the moment the far end wants to *recognise* us.
///
/// It is what broke trusting an admin device. The host remembers the endpoint
/// ids it has approved, checks the next connection against that list, and the
/// list could never match: every dial arrived under a new key, so the owner of
/// the machine was asked to approve their own laptop again on every single
/// connection. Nothing was wrong with the remembering; there was simply nothing
/// stable to remember.
///
/// Set once at startup from the same stored key the host side uses, so a
/// machine is one identity in both directions — which is what "trust this
/// device" has to mean for the sentence to be true.
static OUTGOING_IDENTITY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// Give this process a stable identity for the connections it makes.
///
/// Call once, before dialling anything. Later calls are ignored rather than
/// refused: an endpoint id that changed mid-run would be a second machine as
/// far as every host is concerned, and that is never what a caller means.
pub fn adopt_identity(secret_key: [u8; 32]) {
    if OUTGOING_IDENTITY.set(secret_key).is_err() {
        debug!("An outgoing identity was already adopted; keeping the first one");
    }
}

/// Whether this process has an identity of its own to dial with.
pub fn has_identity() -> bool {
    OUTGOING_IDENTITY.get().is_some()
}

/// The key this machine dials with: its own, and **not** the one it hosts on.
///
/// # Why not simply the machine's key
///
/// It was, and two endpoints then lived under one endpoint id — the host
/// listening and the client dialling — which is not a thing iroh supports and
/// not a thing the relays tolerate. The relay allows one connection per id and
/// boots the other:
///
/// ```text
/// Relay server reports problem: Another endpoint connected with the same
/// endpoint id. No more messages will be received.
/// ```
///
/// Sixty-two of those in one log. Whichever of the two connected last held the
/// slot, so making an outgoing connection knocked this machine's *host* off the
/// relay — and being on the relay is how a distant guest finds it and how hole
/// punching is arranged. A host that works or does not depending on whether
/// anybody has dialled out from it lately is exactly the intermittency this
/// looked like.
///
/// It also made a loopback connection impossible: pasting this machine's own
/// code asked iroh to dial its own id, which it refuses outright.
///
/// # Why derived rather than stored
///
/// Recognition needs the id to be *stable*, not to be the host's — a host that
/// trusts a returning admin device remembers whichever id dialled it. Deriving
/// one from the machine key gives stability with nothing new to store,
/// nothing to migrate, and no second secret to keep safe. The derivation is
/// one-way, so the dialling key never discloses the hosting one.
///
/// **Upgrading asks once more.** A device already trusted was trusted under the
/// old id, and it now arrives under this one. That is one approval per trusted
/// device, once.
fn outgoing_key() -> Option<iroh::SecretKey> {
    let host = OUTGOING_IDENTITY.get()?;
    // Domain-separated: the context string is what stops this key ever
    // coinciding with one derived elsewhere from the same secret.
    Some(iroh::SecretKey::from_bytes(&blake3::derive_key(
        "tain 2026 outgoing endpoint v1",
        host,
    )))
}

/// An endpoint builder carrying this machine's dialling identity, when it has
/// one.
///
/// Without one the dial still works — it is only recognition that is lost, and
/// a host will simply ask about the connection as it would about a stranger.
fn outgoing_builder() -> iroh::endpoint::Builder {
    let builder = super::discovery::endpoint_builder();
    match outgoing_key() {
        Some(key) => builder.secret_key(key),
        None => builder,
    }
}

/// The endpoint this machine dials from, bound once and shared.
///
/// # Why one and not one per connection
///
/// Every dial used to bind its own, which was fine while each carried a fresh
/// throwaway key. With a stable identity it stops being fine: two endpoints
/// live at once under one node id are two machines claiming the same name, and
/// a person with two session tabs open has exactly that. It shows up as the
/// second connection failing for no visible reason.
///
/// One endpoint is also what a client normally is. It is bound on first use,
/// kept for the life of the process, and shared by every session — which is why
/// [`GuestClient::shutdown`] closes its *connection* and leaves this alone.
///
/// The discovery services are held here too. They are what mDNS and the DHT
/// lookups run on, and dropping them would quietly turn both off.
static OUTGOING_ENDPOINT: tokio::sync::OnceCell<(iroh::Endpoint, super::discovery::Attached)> =
    tokio::sync::OnceCell::const_new();

/// Bind — or return — the endpoint this machine dials from.
async fn outgoing_endpoint() -> Result<&'static (iroh::Endpoint, super::discovery::Attached)> {
    OUTGOING_ENDPOINT
        .get_or_try_init(|| async { super::discovery::bind(outgoing_builder()).await })
        .await
}

pub fn must_be_approved(
    typ: PasscodeType,
    endpoint_id: &[u8; 32],
    trusted_admins: &std::collections::BTreeSet<[u8; 32]>,
) -> bool {
    match typ {
        PasscodeType::Guest => true,
        PasscodeType::Master => !trusted_admins.contains(endpoint_id),
    }
}

/// Tunables for an individual guest session.
///
/// `PartialEq` because a running session compares the shared settings against
/// what it last applied, once per evaluation window — see
/// [`spawn_capture_pipeline`]. Comparing is how "nothing changed" stays free.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Screen capture settings (resolution, fps, cursor).
    pub capture: CaptureConfig,
    /// Encoder settings.
    pub encode: EncodeConfig,
    /// How long a guest waits for the host to answer before being refused.
    ///
    /// Nothing is streamed during the wait, so timing out is a denial rather
    /// than a downgrade — an unattended machine must not hand out its screen.
    pub approval_timeout: Duration,
    /// Regenerate the guest passcode once the last guest session disconnects.
    pub auto_regen_guest_passcode: bool,
    /// Executables to keep out of the streamed audio, e.g. "Discord.exe".
    ///
    /// Empty means send everything. Resolved to process ids when the session
    /// starts, because a pid is different every launch.
    pub exclude_audio_processes: Vec<String>,
    /// Opus bitrate in kbit/s. 0 falls back to the codec's default.
    pub audio_bitrate_kbps: u32,
    /// What this machine calls itself, sent to guests so their saved-connection
    /// list reads as machine names instead of hex. `None` stays anonymous.
    pub host_name: Option<String>,
    /// What the adaptive controller gives up first when a stage falls behind.
    ///
    /// Not a quality setting: see [`super::encoder::LatencyPriority`].
    pub priority: super::encoder::LatencyPriority,
    /// Resize a *virtual* display to match the guest watching it.
    ///
    /// # Why this is worth doing at all
    ///
    /// A guest window of 1856x1020 shown a 2560x1600 desktop is scaled twice —
    /// down on the host to fit the link, up on the guest to fill the window —
    /// and text never looks right through either. A display made the size of
    /// the window is drawn 1:1, which is the whole reason a host would create
    /// one for a guest rather than share the screen it already has.
    ///
    /// # Why only a virtual one, and only the first guest's
    ///
    /// A real monitor belongs to whoever is sitting at it. Resizing it because
    /// somebody connected would rearrange a desk nobody asked to have
    /// rearranged, so this never touches one.
    ///
    /// And a display can only be one size, so with two guests watching the same
    /// virtual display the second would take it away from the first, repeatedly,
    /// for as long as both were connected. The first to watch it keeps it; a
    /// second guest wanting their own size wants a second virtual display.
    pub virtual_display_follows_guest: bool,
    /// Endpoints the host has already let in with the admin passcode.
    ///
    /// # Why an admin code is not enough on its own
    ///
    /// The admin passcode is unrestricted access to the machine — every screen,
    /// mouse and keyboard from the first frame, and with the service installed
    /// that includes the lock screen and UAC prompts. Anything that ends up
    /// holding it *is* the owner as far as this host is concerned.
    ///
    /// Which makes a copied code a very bad thing to lose, and codes do get
    /// lost: pasted into the wrong window, read off a screen share, left in a
    /// clipboard manager. Until now the code alone was the whole of the
    /// authorisation, so a leak was silent and immediate.
    ///
    /// Requiring the host to approve the *first* connection from each endpoint
    /// makes the leak visible — somebody has to be at the machine and say yes —
    /// without making the code tedious for the person it belongs to, because
    /// approval is asked once per endpoint and not once per connection.
    ///
    /// # What an endpoint is here
    ///
    /// The guest's iroh endpoint id: the public key its connection is
    /// authenticated with, which cannot be claimed by anybody else. Not a name,
    /// not an address — both of those are things a caller says about itself.
    ///
    /// # What clears it
    ///
    /// Resetting the admin passcode. That is the revocation the interface
    /// already offers and the one people reach for when a code has got out, so
    /// it takes the trust with it — a reset that left a leaked machine trusted
    /// would be worse than useless, because it would look like it had worked.
    pub trusted_admins: std::collections::BTreeSet<[u8; 32]>,
    /// How long remote input gives way after somebody uses this machine.
    ///
    /// Zero turns it off. See [`super::input_inject::Deferring`] for what it
    /// suppresses and what it deliberately does not, and
    /// [`super::local_input`] for how a person is told apart from an injected
    /// event.
    ///
    /// Carried here so it reaches the host from the settings panel, and applied
    /// to the process rather than to the session — one keyboard, one answer.
    pub local_input_wins_for: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            capture: CaptureConfig::default(),
            encode: EncodeConfig::default(),
            approval_timeout: Duration::from_secs(30),
            auto_regen_guest_passcode: true,
            exclude_audio_processes: Vec::new(),
            audio_bitrate_kbps: 160,
            host_name: None,
            priority: crate::LatencyPriority::default(),
            // Off: resizing somebody's desktop because a guest connected is a
            // surprising thing to do without being asked for it.
            virtual_display_follows_guest: false,
            // Nobody, until somebody at this machine says so. A default that
            // trusted anything would make the whole check decorative.
            trusted_admins: std::collections::BTreeSet::new(),
            local_input_wins_for: Duration::from_millis(
                super::local_input::DEFAULT_WINDOW_MS,
            ),
        }
    }
}

impl SessionConfig {
    /// The stream settings this session should run at when nothing is wrong.
    ///
    /// The controller starts here and never exceeds it. `native` is the size the
    /// capture backend actually produces, which is what the default bitrate has
    /// to be derived from — a bitrate picked for 1080p makes a 4K stream look
    /// like a smear, and one picked for 4K wastes a link at 720p.
    pub fn ceiling_plan(&self, native: (u16, u16)) -> super::adaptive::StreamPlan {
        let fps = self.capture.fps.max(1);
        super::adaptive::StreamPlan {
            scale: self.capture.quality,
            fps,
            bitrate_kbps: match self.encode.target_bitrate_kbps {
                // No limit set, so the slider decides how generous to be — see
                // `LatencyPriority::quality_bits_per_pixel`.
                0 => super::encoder::ceiling_bitrate_kbps(native.0, native.1, fps, self.priority),
                kbps => kbps,
            },
        }
    }
}

// ── Host side ────────────────────────────────────────────────

/// A running host server that accepts guest connections via iroh P2P.
pub struct HostServer {
    endpoint: iroh::Endpoint,
    passcode_store: Arc<PasscodeStore>,
    sessions: Arc<SessionManager>,
    /// Channel to notify GUI of pending approval requests from guest passcodes.
    approval_tx: Option<mpsc::UnboundedSender<ApprovalRequest>>,
    /// Channel to notify GUI when a new guest passcode has been generated (auto-regen).
    passcode_changed_tx: Option<mpsc::UnboundedSender<String>>,
    /// Read when a session starts, so settings changes reach new connections
    /// without restarting the server. Live sessions keep what they began with.
    /// The settings new sessions start with, and that running ones follow.
    ///
    /// Shared rather than copied into each session: changing the codec or the
    /// frame rate used to mean disconnecting and reconnecting to try it, which
    /// makes comparing two settings a chore rather than a glance.
    config: Arc<std::sync::Mutex<SessionConfig>>,
}

impl HostServer {
    /// The addresses this host is actually bound to.
    ///
    /// What a guest needs to reach it without asking a relay or a name server
    /// anything — which is what a guest that has connected once already does,
    /// and what a test on a machine with no relay must do.
    pub fn local_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Create a new host with a fresh identity.
    pub async fn bind() -> Result<Self> {
        Self::bind_with_identity(None).await
    }

    /// Create a host, optionally reusing a previously saved identity.
    ///
    /// **The endpoint id is part of every connection code**, so a new identity
    /// invalidates every code this machine has ever handed out. Restoring it is
    /// what makes a saved connection on someone else's machine keep working
    /// after this one restarts.
    pub async fn bind_with_identity(identity: Option<[u8; 32]>) -> Result<Self> {
        // The ALPN list must be declared at bind time. `Endpoint::bind(preset)`
        // leaves it empty, and an endpoint with no ALPNs rejects every incoming
        // connection during the TLS handshake ("peer doesn't support any known
        // protocol") — the listener appears healthy but can never accept.
        let mut builder = super::discovery::endpoint_builder().alpns(vec![ALPN.to_vec()]);
        if let Some(bytes) = identity {
            builder = builder.secret_key(iroh::SecretKey::from_bytes(&bytes));
        }
        // The host is the side worth finding: mDNS is what lets a guest on the
        // same network reach it with no relay and no DNS at all.
        let (endpoint, _lookups) = super::discovery::bind(builder).await?;

        // # Coming online is a courtesy, not a requirement
        //
        // `online()` resolves once the endpoint has reached a relay and has
        // somewhere to be found. This waited for it with no timeout, which on a
        // link where the relays are unreachable — the link this is built for —
        // is a host that never finishes starting: no listener, no log line past
        // this point, and nothing to say why.
        //
        // It is not needed to accept anything. A guest that already knows an
        // address, which after one successful connection it does, reaches this
        // host over the network directly and never involves a relay. So the
        // wait is bounded and its failure is reported rather than fatal — the
        // guest side has always done exactly this.
        if tokio::time::timeout(HOST_ONLINE_WAIT, endpoint.online()).await.is_err() {
            warn!(
                "No relay answered within {}s — this host is listening anyway, and can be \
                 reached by anyone who already knows an address for it",
                HOST_ONLINE_WAIT.as_secs()
            );
        }

        info!("Host endpoint ready");
        info!("  Endpoint ID: {}", endpoint.id());

        Ok(Self {
            endpoint,
            passcode_store: Arc::new(PasscodeStore::new()),
            sessions: Arc::new(SessionManager::new()),
            approval_tx: None,
            passcode_changed_tx: None,
            config: Arc::new(std::sync::Mutex::new(SessionConfig::default())),
        })
    }

    /// Attach an approval request notification channel (used by GUI mode).
    pub fn with_approval_channel(mut self, tx: mpsc::UnboundedSender<ApprovalRequest>) -> Self {
        self.approval_tx = Some(tx);
        self
    }

    /// Attach a passcode-changed notification channel (auto-regen guest codes).
    pub fn with_passcode_changed_channel(mut self, tx: mpsc::UnboundedSender<String>) -> Self {
        self.passcode_changed_tx = Some(tx);
        self
    }

    /// Override session tunables (capture resolution, fps, approval timeout).
    pub fn with_config(self, config: SessionConfig) -> Self {
        *self.config.lock().unwrap() = config;
        self
    }

    /// Replace session tunables while the server is running.
    ///
    /// Applies to sessions started from now on; anything already connected keeps
    /// the configuration it was created with.
    pub fn set_session_config(&self, config: SessionConfig) {
        // Pushed into the process rather than kept here, because that is where
        // it is read from: the machine has one keyboard and one answer to how
        // long its owner gets it to themselves, and applying it here means
        // every session already running follows the new value rather than only
        // the ones started afterwards.
        super::local_input::set_window(config.local_input_wins_for);
        *self.config.lock().unwrap() = config;
    }

    /// Snapshot of the configuration new sessions will use.
    pub fn session_config(&self) -> SessionConfig {
        self.config.lock().unwrap().clone()
    }

    /// The host's iroh EndpointId as a hex string, for display and diagnostics.
    pub fn endpoint_id(&self) -> String {
        format!("{}", self.endpoint.id())
    }

    /// The host's endpoint ID as raw bytes, for embedding in connection codes.
    pub fn endpoint_id_bytes(&self) -> [u8; 32] {
        *self.endpoint.id().as_bytes()
    }

    /// The relay this host is currently reachable through, if any.
    ///
    /// Travels inside every code so a guest never has to look this machine up.
    /// See `connection_code` for why that lookup is the thing most likely to
    /// fail on a restricted network.
    fn home_relay(&self) -> Option<String> {
        use iroh::Watcher as _;
        // `get` needs the watcher by value and mutably; it is a snapshot of a
        // list that is empty until the first relay handshake completes.
        let mut status = self.endpoint.home_relay_status();
        status.get().first().map(|relay| relay.url().to_string())
    }

    /// Wrap a secret into a shareable code carrying this host's address.
    fn code_for(&self, typ: PasscodeType, secret: [u8; 32]) -> String {
        ConnectionCode::with_relay(typ, secret, self.endpoint_id_bytes(), self.home_relay())
            .encode()
    }

    /// Shared session table — lets the GUI observe and control live sessions.
    pub fn sessions(&self) -> Arc<SessionManager> {
        Arc::clone(&self.sessions)
    }

    /// Generate a master connection code — grants immediate full control to any holder.
    pub async fn generate_master_passcode(&self) -> Result<String> {
        let secret = self.passcode_store.generate(PasscodeType::Master).await?;
        let code = self.code_for(PasscodeType::Master, secret);
        info!("Generated MASTER code: {}", redact(&code));
        Ok(code)
    }

    /// Re-register a code that was saved from a previous run.
    ///
    /// Returns the shareable string, which is only the same as last time if the
    /// endpoint identity was also restored — the endpoint id is baked into the
    /// code. See [`HostServer::bind_with_identity`].
    pub async fn adopt_passcode(&self, typ: PasscodeType, secret: [u8; 32]) -> String {
        self.passcode_store.adopt(secret, typ).await;
        let code = self.code_for(typ, secret);
        info!("Restored {} code: {}", typ.prefix(), redact(&code));
        code
    }

    /// This endpoint's secret key, so it can be restored on the next run.
    pub fn identity_bytes(&self) -> [u8; 32] {
        self.endpoint.secret_key().to_bytes()
    }

    /// Generate a guest connection code — requires host approval before input is forwarded.
    pub async fn generate_guest_passcode(&self) -> Result<String> {
        let secret = self.passcode_store.generate(PasscodeType::Guest).await?;
        let code = self.code_for(PasscodeType::Guest, secret);
        info!("Generated GUEST code: {}", redact(&code));
        Ok(code)
    }

    /// Replace the code of the given type, invalidating the previous one.
    pub async fn regenerate_passcode(&self, typ: PasscodeType) -> Result<String> {
        let secret = self.passcode_store.regenerate(typ).await?;
        let code = self.code_for(typ, secret);
        info!("Regenerated {} code: {}", typ.prefix(), redact(&code));
        if let (PasscodeType::Guest, Some(tx)) = (typ, &self.passcode_changed_tx) {
            let _ = tx.send(code.clone());
        }
        Ok(code)
    }

    /// All currently valid codes, rendered in shareable form.
    pub async fn list_passcodes(&self) -> Vec<(String, PasscodeType)> {
        self.passcode_store
            .list_codes()
            .await
            .into_iter()
            .map(|(secret, typ)| (self.code_for(typ, secret), typ))
            .collect()
    }

    /// Revoke a code, given its shareable string.
    pub async fn revoke_passcode(&self, code: &str) -> Result<()> {
        let parsed = ConnectionCode::parse(code)?;
        self.passcode_store.revoke(&parsed.secret).await
    }

    /// Set which input devices a live session may drive.
    ///
    /// Takes effect immediately — the session's input path re-reads grants on
    /// every event, so this works long after the initial approval window closed —
    /// and the guest is notified via `Message::ControlState`.
    /// Returns false if the session is not (or no longer) active.
    pub fn approve_session(&self, session_id: u32, grants: ControlGrants) -> bool {
        self.sessions.set_control(session_id, grants)
    }

    /// Change what a live session may see.
    ///
    /// Takes effect immediately, in both directions: the permission bounds what
    /// the guest may ask for from the next request onward, and the stream moves
    /// if what it was showing is no longer permitted. The guest is told through
    /// `Message::SharedChanged`. Returns false if the session is not (or no
    /// longer) active.
    ///
    /// `available` is this machine's displays, passed in because the session
    /// table has no business enumerating them.
    pub fn share_with_session(
        &self,
        session_id: u32,
        displays: super::protocol::DisplayGrant,
        available: &[super::protocol::DisplayInfo],
    ) -> bool {
        self.sessions.set_displays(session_id, displays, available)
    }

    /// What a live session may currently see, for a dialog that offers to
    /// change it.
    pub fn shared_with_session(
        &self,
        session_id: u32,
    ) -> Option<super::protocol::DisplayGrant> {
        self.sessions.displays_of(session_id)
    }

    /// Disconnect a guest from the host side.
    ///
    /// `reason` is one of [`super::protocol::end_reason`] — see `SessionEnd`.
    pub fn kick_session(&self, session_id: u32, reason: &str) -> bool {
        self.sessions.kick(session_id, reason)
    }

    /// Start accepting connections. Runs until the endpoint is closed.
    ///
    /// Takes `Arc<Self>` so the caller keeps a handle for passcode management
    /// and session approval while the server runs in a spawned task.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!(
            "Listening for incoming connections on ALPN {:?}",
            std::str::from_utf8(ALPN).unwrap_or("<invalid>")
        );

        loop {
            // iroh 1.x: accept().await returns Option<Incoming>. None means endpoint closed.
            let Some(incoming) = self.endpoint.accept().await else { break };

            // # Nothing per-connection may happen on this loop
            //
            // `incoming.await` is the QUIC handshake, and it used to be awaited
            // right here. That made the whole server single-file: one guest
            // could be shaking hands at a time, and anyone else who dialled
            // waited behind them — not for a moment, but until that handshake
            // either finished or gave up.
            //
            // On a link where handshakes routinely stall, which is the link this
            // is for, that is most of them. The log shows what it looks like
            // from here: "Failed to accept QUIC connection: timed out", and
            // every attempt that arrived during the wait is gone. From the
            // guest it looks like the host ignoring them at random, including
            // while the card says it is online — because the reachability sweep
            // dials every saved host too, and those handshakes were queueing in
            // front of the one the user actually asked for.
            //
            // So the handshake is spawned with the connection it belongs to.
            // The loop does nothing now but hand work off, which is the only
            // thing an accept loop should ever do.
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                // A failed handshake must not take the whole server down.
                let conn = match incoming.await {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!("Failed to accept QUIC connection: {}", e);
                        return;
                    }
                };

                if conn.alpn() != ALPN.to_vec() {
                    info!("Rejected non-RDP connection (ALPN: {:?})", conn.alpn());
                    return;
                }

                // # The same refusal the guest makes, made here too
                //
                // The guest is waiting on this at the same moment, so in the
                // normal case both sides settle together and this costs
                // nothing. It is here anyway for the two cases where the guest
                // does not decide it: an older build on the other end, and a
                // host that would otherwise spend the wait starting a capture,
                // an encoder and an audio stream for a session that is about to
                // be refused from the far side.
                match await_direct_path(&conn, DIRECT_PATH_WAIT + HOST_PUNCH_MARGIN).await {
                    DirectPath::Established(path) => {
                        info!("Guest connected directly: {}", path.remote());
                    }
                    DirectPath::StillRelayed { via } => {
                        warn!(
                            "A guest reached this host only through the relay {via} and no \
                             direct path opened — refusing the connection rather than \
                             streaming over a relay. The guest should have given up first \
                             and been told why; that it did not suggests an older build on \
                             the other end."
                        );
                        conn.close(0u32.into(), b"relay-only");
                        return;
                    }
                    DirectPath::Lost => {
                        debug!("A guest connection ended before a direct path opened");
                        return;
                    }
                }

                if let Err(e) = server.handle_connection(conn).await {
                    error!("Connection handler error: {}", e);
                }
            });
        }

        info!("Host server shutting down");
        Ok(())
    }

    /// Close the host server and all active connections.
    pub async fn shutdown(&self) -> Result<()> {
        self.endpoint.close().await;
        Ok(())
    }

    // ── Internal connection handler ────────────────────────

    /// Handle a single incoming QUIC connection from a guest.
    async fn handle_connection(self: Arc<Self>, conn: iroh::endpoint::Connection) -> Result<()> {
        // Host accepts; the guest is the side that opens.
        let (mut send, mut recv) = conn.accept_bi().await.context("Failed to accept bi stream")?;

        let Some(msg_bytes) = read_stream_message(&mut recv).await? else {
            anyhow::bail!("Connection closed before auth message");
        };

        // The session id in the guest's Auth message is advisory only — we assign
        // our own. Trusting the guest's id would let it collide with, or claim the
        // approval state of, another live session.
        let (passcode, client_name) = match bincode::deserialize::<Message>(&msg_bytes)? {
            Message::Auth { passcode, client_name, .. } => (
                passcode,
                // Sanitized, never trusted: this string is drawn in the host's
                // own UI next to a button that hands over the machine.
                client_name.as_deref().and_then(super::protocol::sanitize_device_name),
            ),
            // A probe is answered and closed here, before any of the session
            // machinery. Nothing is registered, nobody is prompted.
            Message::Probe(probe) => {
                return self.answer_probe(&mut send, &probe.passcode).await;
            }
            other => {
                warn!("Expected Auth as first message, got: {:?}", other);
                anyhow::bail!("Invalid first message");
            }
        };

        // The guest sends its whole connection code; authentication turns on the
        // secret inside it, not on the string.
        let parsed = ConnectionCode::parse(&passcode).ok();
        if let Some(code) = parsed.as_ref().filter(|c| c.endpoint_id != self.endpoint_id_bytes())
        {
            warn!("Code {} addresses a different host — rejecting", code.redacted());
        }

        let validated = match parsed {
            Some(ref code) if code.endpoint_id == self.endpoint_id_bytes() => self
                .passcode_store
                .validate(&code.secret)
                .await
                .map(|typ| (typ, code.secret)),
            _ => None,
        };

        let Some((typ, secret)) = validated else {
            warn!("Rejected invalid connection code: {}", redact(&passcode));
            write_stream_message(
                &mut send,
                &Message::SessionEnd(super::protocol::SessionEnd {
                    session_id: 0,
                    reason: super::protocol::end_reason::INVALID_PASSCODE.into(),
                    args: Vec::new(),
                }),
            )
            .await?;

            // Returning here drops `conn`, which tears down the QUIC connection
            // immediately — the rejection would race the close and the guest would
            // just see "connection lost" with no reason. Wait for the peer to
            // acknowledge the write first.
            let _ = send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
            return Ok(());
        };

        // Master codes are the "this is my own machine" path and grant control
        // outright. Guests start with nothing and are not streamed to at all
        // until the host says otherwise.
        let initial_grants = match typ {
            PasscodeType::Master => ControlGrants::ALL,
            PasscodeType::Guest => ControlGrants::NONE,
        };
        let session_id = self.sessions.add_session(secret, initial_grants, typ);
        let label = client_name
            .clone()
            .unwrap_or_else(|| super::i18n::format("session.guest_number", &[("n", &session_id.to_string())]));
        self.sessions.set_label(session_id, label.clone());
        // Recorded once. The address does not change for the life of the
        // connection, and the number that does — the round trip — comes back
        // from the guest continuously and is kept separately.
        if let Some(path) = path_of(&conn) {
            self.sessions.set_route(session_id, format!("{} — {}", path.label(), path.remote()));
        }
        info!(
            "Session #{} authenticated as {:?} ({} passcode)",
            session_id,
            label,
            typ.prefix()
        );

        // Everything past this point runs inside a helper so that `end_session`
        // is reached on *every* exit, including the error ones. A `?` between
        // registering the session and cleaning it up would leave a dead entry in
        // the table: the host's UI would show a pill for a guest that is gone,
        // and `guest_session_count` would never reach zero, so the guest code
        // would never regenerate. The approval wait makes that window long
        // enough for a guest to give up and vanish mid-way.
        let result = self
            .clone()
            .run_authenticated(
                conn, send, recv, session_id, typ, initial_grants, label, client_name,
            )
            .await;

        if let Err(ref e) = result {
            warn!("Session #{} ended with error: {}", session_id, e);
        }
        self.end_session(session_id).await;
        result
    }

    /// Answer a reachability probe and close, without touching session state.
    ///
    /// The point of the probe is that a saved-connection card can show a live,
    /// honest state — is this host up, is this code still current — without
    /// putting an approval prompt on someone else's screen to find out. So this
    /// path must remain free of side effects: no session, no approval channel,
    /// no log line the host's user would read as a connection attempt.
    ///
    /// An invalid code is answered after a fixed pause. A probe is cheaper to
    /// send than an authentication, so without it this would be the fastest way
    /// to test guesses against a host — still hopeless against a 32-byte secret,
    /// but there is no reason to hand out the faster oracle.
    async fn answer_probe(
        self: &Arc<Self>,
        send: &mut iroh::endpoint::SendStream,
        passcode: &str,
    ) -> Result<()> {
        let validated = match ConnectionCode::parse(passcode).ok() {
            Some(code) if code.endpoint_id == self.endpoint_id_bytes() => {
                self.passcode_store.validate(&code.secret).await
            }
            _ => None,
        };

        let result = match validated {
            Some(typ) => super::protocol::ProbeResult {
                valid: true,
                host_name: self.session_config().host_name.clone(),
                needs_approval: typ == PasscodeType::Guest,
            },
            None => {
                tokio::time::sleep(INVALID_PROBE_DELAY).await;
                super::protocol::ProbeResult {
                    valid: false,
                    // Nothing about this machine to a caller who did not have a
                    // working code.
                    host_name: None,
                    needs_approval: false,
                }
            }
        };

        debug!("Answered a probe: valid={}", result.valid);
        write_stream_message(send, &Message::ProbeResult(result)).await?;
        let _ = send.finish();
        // Same reasoning as the rejection paths: returning drops the connection,
        // and the answer would race the close.
        let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
        Ok(())
    }

    /// Approve, announce, and run a session that has already authenticated.
    ///
    /// The caller owns cleanup — see the comment at its call site.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn run_authenticated(
        self: Arc<Self>,
        conn: iroh::endpoint::Connection,
        mut send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
        session_id: u32,
        typ: PasscodeType,
        initial_grants: ControlGrants,
        label: String,
        client_name: Option<String>,
    ) -> Result<()> {
        // Snapshot once: this session runs with the settings in force when it
        // started, even if the user changes them mid-session.
        let config = self.session_config();
        let live_config = Arc::clone(&self.config);

        // A master code is the machine's own owner and sees every screen; a
        // guest sees what the approval dialog chose, which replaces this.
        let mut allowed = super::protocol::DisplayGrant::All;

        // # Who has to be asked about
        //
        // Every guest, every time — that has always been true, and what the
        // host grants is theirs to decide per connection.
        //
        // And an admin arriving from an endpoint this machine has not seen
        // before. Not every time: approval is remembered per endpoint, so the
        // owner answers once from each of their own devices and never again.
        // What it catches is a code that has got out — see
        // [`SessionConfig::trusted_admins`], which is where the whole reasoning
        // lives.
        let endpoint_id = *conn.remote_id().as_bytes();
        let is_admin = typ == PasscodeType::Master;
        let must_ask = must_be_approved(typ, &endpoint_id, &config.trusted_admins);
        if is_admin && !must_ask {
            debug!("Session #{session_id} is an admin endpoint this host already trusts");
        }

        // Guests wait here. Nothing has been captured, encoded, or sent — a
        // refused guest never sees a pixel of this machine.
        let grants = if must_ask {
            write_stream_message(
                &mut send,
                &Message::GuestApprovalRequest(super::protocol::GuestApprovalRequest {
                    session_id,
                    label: label.clone(),
                }),
            )
            .await?;

            let decision = self
                .await_approval(
                    session_id,
                    label,
                    client_name.is_some(),
                    is_admin,
                    endpoint_id,
                    config.approval_timeout,
                )
                .await;

            match decision {
                ApprovalDecision::Allow { grants, displays } => {
                    // # An approved admin keeps what an admin has
                    //
                    // The dialog's grants belong to the guest question — which
                    // screens, whether the mouse is forwarded. There is no
                    // version of that question for an admin: the code already
                    // carries unrestricted access, and handing back less than
                    // it authorises would be an interface pretending to a
                    // limit the protocol does not have. The only thing being
                    // decided here is whether this endpoint is the owner.
                    let grants = if is_admin { initial_grants } else { grants };
                    if !is_admin {
                        allowed = displays;
                    }
                    self.sessions.set_control(session_id, grants);
                    grants
                }
                ApprovalDecision::Deny => {
                    // # Which refusal this was
                    //
                    // Determined here rather than carried out of
                    // `await_approval`, because the condition is the same one
                    // it tested and this keeps `ApprovalDecision` a decision
                    // rather than a decision plus an explanation.
                    //
                    // An admin refused by a host with no approval channel was
                    // never asked about — there was nobody to ask. Saying "the
                    // host did not allow this connection" points the owner at a
                    // decision that was never made, and the usual next thought
                    // is that the code is wrong. See `end_reason::NOT_APPROVED`.
                    let reason = if is_admin && self.approval_tx.is_none() {
                        super::protocol::end_reason::NOT_APPROVED
                    } else {
                        super::protocol::end_reason::NOT_ALLOWED
                    };
                    info!("Session #{} was refused by the host ({reason})", session_id);
                    write_stream_message(
                        &mut send,
                        &Message::SessionEnd(super::protocol::SessionEnd {
                            session_id,
                            reason: reason.into(),
                            args: Vec::new(),
                        }),
                    )
                    .await?;
                    // Same reasoning as the invalid-passcode path: returning
                    // drops the connection and the reason would race the close.
                    let _ = send.finish();
                    let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                    return Ok(());
                }
            }
        } else {
            initial_grants
        };

        // # A window grant overrides what the host settings capture
        //
        // The settings say what this machine shares by default; a grant says
        // what *this guest* was allowed. When the two disagree the grant wins,
        // because it is the more specific and the more recent — somebody chose
        // it, for this connection, in the approval dialog.
        //
        // Applied to the snapshot rather than at each use, so every later
        // reader — the capture thread, the display switch, the pointer mapping
        // — sees one answer and cannot disagree with another.
        //
        // Several windows open on all of them at once, matching what
        // `DisplayGrant::opens_on` tells the guest to expect — the two disagreeing
        // would show one window while the switcher claimed the composite.
        let mut config = config;
        match allowed.windows() {
            [] => {}
            [only] => {
                config.capture.display = super::screen_capture::DisplayTarget::Window(*only);
            }
            several => {
                config.capture.display =
                    super::screen_capture::DisplayTarget::Windows(several.to_vec());
            }
        }
        let config = config;

        // Build the capturer *before* announcing the session. The backend is the
        // authority on frame size — a display that changed since the config was
        // written would otherwise make SessionStart advertise dimensions the
        // frames do not match.
        let capturer = match super::screen_capture::create_capturer(config.capture.clone()) {
            Ok(capturer) => capturer,
            Err(e) => {
                // Told, not dropped. Capture fails for reasons the guest can act
                // on — the host is at its lock screen, or a UAC prompt is up —
                // and a connection that simply vanishes looks like a network
                // fault, which sends them looking in the wrong place entirely.
                error!("Session #{session_id} could not start capturing: {e:#}");
                // A failure with a remedy already carries the code for it, and
                // the guest is the only screen that will ever show it. Anything
                // else is the operating system's own words, which no catalogue
                // can carry and which travel as an argument instead.
                let (reason, args) = match super::screen_capture::Remedy::in_chain(&e) {
                    Some(remedy) => (remedy.code.to_string(), remedy.args.clone()),
                    None => (
                        super::protocol::end_reason::CAPTURE_FAILED.to_string(),
                        vec![("detail".to_string(), format!("{e}"))],
                    ),
                };
                write_stream_message(
                    &mut send,
                    &Message::SessionEnd(super::protocol::SessionEnd {
                        session_id,
                        reason,
                        args,
                    }),
                )
                .await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                return Ok(());
            }
        };
        let (capture_w, capture_h) = capturer.frame_size();

        // # Said by the host, not only by the guest
        //
        // The guest logs what it was told, which answers "did the list arrive".
        // It cannot answer "did this machine see the monitor", and that is the
        // question a host with a screen it expected to share actually has —
        // asked, the first time, of a dummy HDMI plug that produced no menu on
        // the far end with nothing anywhere saying why.
        // Only what this session was allowed. A guest offered screens it may
        // not watch would have a menu whose entries fail on click, which is
        // worse than a shorter menu.
        let available_displays: Vec<_> = super::screen_capture::displays()
            .into_iter()
            .filter(|d| allowed.allows(d.id))
            .collect();
        if available_displays.is_empty() {
            warn!(
                "Session #{session_id}: this host could not enumerate any display, so the \
                 guest gets no choice of screen"
            );
        } else {
            info!(
                "Session #{session_id}: offering {} display(s): {}",
                available_displays.len(),
                available_displays
                    .iter()
                    .map(|d| format!(
                        "{} {}x{} at ({}, {}){}{}",
                        d.name,
                        d.width,
                        d.height,
                        d.x,
                        d.y,
                        if d.primary { " primary" } else { "" },
                        if d.virtual_display { " virtual" } else { "" },
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        write_stream_message(
            &mut send,
            &Message::SessionStart(super::protocol::SessionStart {
                session_id,
                // effective_codec, not codec: never advertise an encoder we
                // aren't actually running.
                encoder_type: config.encode.effective_codec().to_string(),
                screen_info: super::protocol::ScreenInfo {
                    width: capture_w,
                    height: capture_h,
                    dpi_scale: 1.0,
                },
                host_name: config.host_name.clone(),
                // Every display, for now. Which of them a given guest may see
                // is a permission, and permissions are chosen in the approval
                // dialog — so this is filtered once there is something to
                // filter it by.
                displays: available_displays,
                // Sent from the first message rather than waited for as a
                // change: a window that is *already* minimised when the guest
                // arrives has no transition to announce, and would otherwise be
                // a blank window with nothing saying why.
                //
                // `None` for a composite. This field names the *one* window
                // being watched, and a session showing several is not watching
                // one — filling it with the first would put "waiting for Excel
                // to be restored" over a picture that is showing three other
                // windows perfectly well.
                window: match allowed.windows() {
                    [only] => Some(super::window_capture::describe(*only, None)),
                    _ => None,
                },
                // Asked of each granted handle rather than filtered out of the
                // shareable list, because that list is what a person could
                // *pick* and excludes minimised windows — which a granted
                // window is perfectly entitled to be. One that has closed since
                // the grant drops out here, and the guest never offers it.
                windows: allowed
                    .windows()
                    .iter()
                    .filter_map(|id| super::window_capture::on_offer(*id))
                    .collect(),
                // From the *grant*, not from the list above: a session that
                // shares windows still shares windows when every one of them
                // has been closed, and that is exactly the case the guest
                // needs to be able to name.
                sharing_windows: !allowed.windows().is_empty(),
            }),
        )
        .await?;

        // Tell the guest its permissions up front, so it never has to infer
        // them from which of its events get silently dropped.
        write_stream_message(
            &mut send,
            &Message::ControlState(ControlStateMessage { session_id, grants }),
        )
        .await?;

        // # A second stream, for audio only
        //
        // QUIC delivers each stream in order, so anything sharing one waits
        // behind whatever is in front of it. A keyframe is tens of kilobytes
        // and an Opus packet is about two hundred bytes — putting them on one
        // stream makes every keyframe a hiccup in the sound.
        //
        // Unidirectional because nothing comes back this way, and failure is
        // not fatal: `None` falls back to the main stream, which is how audio
        // worked before this and is merely worse rather than broken.
        let audio_send = match open_tagged_stream(&conn, super::protocol::stream_kind::AUDIO).await
        {
            Ok(stream) => Some(stream),
            Err(e) => {
                debug!("No separate audio stream ({e}) — sharing the control stream");
                None
            }
        };

        // Frames get their own, for the reason at the top of this file. Failing
        // is survivable in the same way audio's is: they fall back to the
        // control stream, which is how this worked before and is merely worse.
        let video_send = match open_tagged_stream(&conn, super::protocol::stream_kind::VIDEO).await
        {
            Ok(stream) => Some(stream),
            Err(e) => {
                debug!("No separate video stream ({e}) — sharing the control stream");
                None
            }
        };

        run_session(
            send,
            recv,
            audio_send,
            video_send,
            session_id,
            Arc::clone(&self.sessions),
            config,
            live_config,
            capturer,
            allowed,
        )
        .await
    }

    /// Ask the host to approve a guest and wait for the answer.
    ///
    /// Every path that is not an explicit "allow" denies. A host that cannot be
    /// asked — no UI attached, receiver gone, nobody at the keyboard — must not
    /// end up streaming its screen by default.
    #[allow(clippy::too_many_arguments)]
    async fn await_approval(
        self: &Arc<Self>,
        session_id: u32,
        label: String,
        label_is_self_reported: bool,
        is_admin: bool,
        endpoint_id: [u8; 32],
        timeout: Duration,
    ) -> ApprovalDecision {
        let Some(ref tx) = self.approval_tx else {
            // Nothing to ask, so nothing can be allowed. Said differently for
            // an admin because the reason somebody would hit it is different:
            // a guest being refused by a host with no interface is the expected
            // shape of that host, while an admin being refused is a machine
            // whose owner cannot get into it, and they need to know it is the
            // missing interface and not their code.
            if is_admin {
                warn!(
                    "Session #{session_id} used the admin passcode from an endpoint this \
                     host has not approved before, and there is no interface here to ask. \
                     Refusing. Connect once from a Tain window on this machine to approve \
                     the device."
                );
            } else {
                warn!("Session #{session_id}: no approval channel attached — refusing the guest");
            }
            return ApprovalDecision::Deny;
        };

        let (approve_tx, approve_rx) = tokio::sync::oneshot::channel();
        let req = ApprovalRequest {
            session_id,
            label,
            label_is_self_reported,
            is_admin,
            endpoint_id,
            approve_tx,
        };
        if tx.send(req).is_err() {
            warn!("Session #{}: approval receiver dropped — refusing the guest", session_id);
            return ApprovalDecision::Deny;
        }

        match tokio::time::timeout(timeout, approve_rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                // Sender dropped without deciding — the UI went away.
                debug!("Session #{}: approval channel closed undecided", session_id);
                ApprovalDecision::Deny
            }
            Err(_) => {
                info!(
                    "Session #{}: nobody answered within {:?} — refusing the guest",
                    session_id, timeout
                );
                ApprovalDecision::Deny
            }
        }
    }

    /// Tear down session state and auto-regenerate the guest passcode if this was the last guest.
    async fn end_session(&self, session_id: u32) {
        let removed = self.sessions.remove_session(session_id);
        let was_guest = removed.is_some_and(|s| s.passcode_type == PasscodeType::Guest);

        if was_guest
            && self.session_config().auto_regen_guest_passcode
            && self.sessions.guest_session_count() == 0
        {
            match self.passcode_store.regenerate(PasscodeType::Guest).await {
                Ok(secret) => {
                    let code = self.code_for(PasscodeType::Guest, secret);
                    info!("Last guest disconnected — regenerated guest code: {}", redact(&code));
                    if let Some(ref tx) = self.passcode_changed_tx {
                        let _ = tx.send(code);
                    }
                }
                Err(e) => warn!("Failed to regenerate guest code: {}", e),
            }
        }
    }
}

// ── Session loop ─────────────────────────────────────────────

/// Drive one authenticated session to completion.
///
/// Open a unidirectional stream and write the byte that says what it carries.
///
/// The tag goes out immediately rather than with the first message, so a guest
/// can dispatch the stream before anything is on it. A stream that is opened
/// and then silent — audio on a machine with no sound device — still announces
/// itself, which is what stops it being mistaken for the other one.
async fn open_tagged_stream(
    conn: &iroh::endpoint::Connection,
    kind: u8,
) -> Result<iroh::endpoint::SendStream> {
    let mut stream = conn.open_uni().await?;
    stream.write_all(&[kind]).await?;
    Ok(stream)
}

/// Generic over the stream halves so it can be exercised over `tokio::io::duplex`
/// in tests without standing up an iroh endpoint.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<W, R, A, V>(
    mut send: W,
    recv: R,
    audio_send: Option<A>,
    video_send: Option<V>,
    session_id: u32,
    sessions: Arc<SessionManager>,
    config: SessionConfig,
    live_config: LiveConfig,
    capturer: Box<dyn super::screen_capture::ScreenCaptureBackend>,
    // Which of the host's screens this session may see. Decided when it was
    // allowed in; a master code gets every screen.
    allowed: super::protocol::DisplayGrant,
) -> Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin + Send + 'static,
    R: tokio::io::AsyncReadExt + Unpin + Send + 'static,
    A: tokio::io::AsyncWriteExt + Unpin + Send + 'static,
    V: tokio::io::AsyncWriteExt + Unpin + Send + 'static,
{
    info!("Session #{} running", session_id);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    // Let the session table push host-initiated messages (grant changes) to this guest.
    sessions.register_outbound(session_id, out_tx.clone());
    let telemetry: SharedTelemetry = Arc::new(std::sync::Mutex::new(None));
    let acks: SharedAck = Arc::new(std::sync::Mutex::new(AckState::default()));
    let viewport: SharedViewport = Arc::new(std::sync::Mutex::new(None));
    let keyframe_wanted: KeyframeWanted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Which display this session streams. Starts as whatever the config asked
    // for, which is `None` — the primary.
    // One injector for the session, not one per thread: the capture thread has
    // to update what the pointer maps against when the desk is rearranged, and
    // `set_shown` takes `&self` precisely so that it can be shared.
    //
    // Wrapped so that remote input stands aside while somebody is using this
    // machine — see [`super::input_inject::Deferring`], which explains why the
    // rule is about presses rather than about all events.
    let injector: Arc<dyn super::input_inject::InputInjector> =
        Arc::new(super::input_inject::create_deferring_injector());
    let wanted_display: WantedDisplay =
        Arc::new(std::sync::Mutex::new(config.capture.display.clone()));
    // Which screens it is *allowed* to stream. A master code is the machine's
    // owner and gets all of them; a guest gets what the approval dialog chose,
    // which is written here when the decision arrives.
    let allowed_displays: AllowedDisplays = Arc::new(std::sync::Mutex::new(allowed));
    // Handed to the session table so the person at this machine can change what
    // is shared while the session runs, rather than only when it starts. Both
    // handles, because narrowing a grant has to move the picture as well — see
    // `SessionManager::set_displays`.
    sessions.register_shows(
        session_id,
        super::session::Shows {
            allowed: Arc::clone(&allowed_displays),
            wanted: Arc::clone(&wanted_display),
        },
    );
    let mut reader = tokio::spawn(run_reader(
        recv,
        session_id,
        Arc::clone(&sessions),
        out_tx.clone(),
        Arc::clone(&telemetry),
        Arc::clone(&acks),
        Arc::clone(&viewport),
        Arc::clone(&keyframe_wanted),
        Arc::clone(&wanted_display),
        Arc::clone(&allowed_displays),
        config.virtual_display_follows_guest,
        Arc::clone(&injector),
    ));

    // Audio rides the same outbound channel as host-initiated control messages,
    // which is deliberate: that path is not gated by the frame ack window. The
    // window exists to stop the host running ahead of the guest's *video*
    // decoder, and making sound wait on video decode would be exactly backwards
    // — a soft picture is tolerable, a stutter in the audio is not.
    // Audio goes down its own channel so it can be written by its own task on
    // its own stream, rather than queueing behind a keyframe.
    let (audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Message>();
    let audio_writer = audio_send.map(|mut stream| {
        tokio::spawn(async move {
            while let Some(msg) = audio_rx.recv().await {
                if write_stream_message(&mut stream, &msg).await.is_err() {
                    break;
                }
            }
            let _ = stream.shutdown().await;
        })
    });
    // Without a second stream, audio shares the main one — worse, not broken.
    let audio_sink = if audio_writer.is_some() { audio_tx } else { out_tx.clone() };

    let audio = spawn_audio_pipeline(
        session_id,
        audio_sink,
        &config.exclude_audio_processes,
        config.audio_bitrate_kbps,
    );

    let encoder = super::encoder::create_encoder(config.encode.clone())?;
    // The pointer goes out on the session's outbound channel, not the frame
    // path: it changes on acquires where the desktop does not, and holding it
    // behind the ack window would make it stutter exactly when the link is
    // worst — which is the one thing the user watches continuously.
    let (cursor_tx, mut cursor_rx) = mpsc::unbounded_channel();
    let (mut frame_rx, pipeline) = spawn_capture_pipeline(
        session_id,
        capturer,
        encoder,
        config,
        live_config,
        telemetry,
        acks,
        viewport,
        Arc::clone(&keyframe_wanted),
        cursor_tx,
        Arc::clone(&wanted_display),
        Arc::clone(&allowed_displays),
        Arc::clone(&injector),
        out_tx.clone(),
    );

    // Written from the select loop rather than handed to a task of its own,
    // which audio can afford and video cannot. Awaiting the write *is* the
    // backpressure: it stops the pipeline producing frames faster than the link
    // carries them. A task behind an unbounded channel would accept every frame
    // instantly and queue pictures that are stale before they leave.
    let mut video_send = video_send;

    let result = loop {
        tokio::select! {
            // The session table also holds a clone of the outbound sender, so the
            // channel closing is no longer a reliable "guest is gone" signal —
            // watch the reader task itself.
            _ = &mut reader => break Ok(()),
            outbound = out_rx.recv() => {
                match outbound {
                    Some(msg) => {
                        // A SessionEnd is the last thing this session sends —
                        // it is how a host-side kick reaches the guest.
                        let closing = matches!(msg, Message::SessionEnd(_));
                        if let Err(e) = write_stream_message(&mut send, &msg).await {
                            break Err(e);
                        }
                        if closing {
                            break Ok(());
                        }
                    }
                    None => break Ok(()),
                }
            }
            update = cursor_rx.recv() => {
                let Some(update) = update else { break Ok(()) };
                if let Err(e) = write_stream_message(&mut send, &Message::Cursor(update)).await {
                    debug!("Session #{}: cursor send failed: {}", session_id, e);
                    break Err(e);
                }
            }
            event = frame_rx.recv() => {
                let msg = match event {
                    Some(PipelineEvent::Frame(encoded)) => {
                        Message::Frame(super::protocol::FrameMessage {
                            sequence_number: encoded.sequence_number,
                            width: encoded.width,
                            height: encoded.height,
                            timestamp_ms: encoded.timestamp_ms,
                            is_keyframe: encoded.is_keyframe,
                            data: encoded.data.to_vec(),
                        })
                    }
                    Some(PipelineEvent::Status(status)) => {
                        sessions.set_stream_status(session_id, status);
                        Message::StreamStatus(status)
                    }
                    Some(PipelineEvent::CodecChanged(encoder_type)) => {
                        Message::CodecChanged(super::protocol::CodecChanged { encoder_type })
                    }
                    Some(PipelineEvent::Preview(picture)) => Message::HostPreview(picture),
                    Some(PipelineEvent::Failed(e)) => break Err(e),
                    // The pipeline thread stopped on its own.
                    None => break Ok(()),
                };

                // Frames down the video stream; status, codec changes and
                // previews stay with the rest of the control traffic, because
                // they are rare and ordering them against the session's other
                // messages is worth more than the stream separation.
                let sent = match (&mut video_send, &msg) {
                    (Some(video), Message::Frame(_)) => write_stream_message(video, &msg).await,
                    _ => write_stream_message(&mut send, &msg).await,
                };
                if let Err(e) = sent {
                    debug!("Session #{}: send failed: {}", session_id, e);
                    break Err(e);
                }
            }
        }
    };

    reader.abort();
    if let Some(audio) = audio {
        audio.stop();
    }
    if let Some(writer) = audio_writer {
        writer.abort();
    }
    if let Some(mut video) = video_send {
        let _ = video.shutdown().await;
    }
    // Dropping the receiver closes the channel, which is how the pipeline thread
    // is told to stop: its next send fails and it returns.
    drop(frame_rx);
    let _ = pipeline.join();
    let _ = send.shutdown().await;
    info!("Session #{} ended", session_id);
    result
}

/// Capacity of the encoded-frame channel.
///
/// Two, not more: one frame in flight to the network and one ready behind it is
/// enough to keep the encoder busy, and every extra slot is a whole frame of
/// added latency when the link is the bottleneck. Zero-capacity would serialise
/// encode and send, losing the overlap this exists for.
const FRAME_QUEUE_DEPTH: usize = 2;

/// How often the pipeline's measurements are handed to the adaptive controller.
///
/// Long enough that a single slow frame does not dominate the window, short
/// enough that a link going bad is answered within about a second.
const EVAL_WINDOW: Duration = Duration::from_millis(750);

/// Guest telemetry, shared from the reader task into the capture thread.
///
/// The reader owns the stream and the pipeline owns the controller, and they
/// live on different threads; this is the whole of the coupling between them.
type SharedTelemetry = Arc<std::sync::Mutex<Option<super::protocol::GuestTelemetry>>>;

/// What the guest has confirmed it displayed, and when it last said so.
#[derive(Debug, Default)]
pub struct AckState {
    sequence: u32,
    /// Frames sent and not yet matched to an ack, oldest first.
    in_flight: std::collections::VecDeque<(u32, std::time::Instant)>,
    /// Smoothed time from sending a frame to its ack arriving.
    last_latency_ms: Option<f32>,
    last_at: Option<std::time::Instant>,
    /// When this session started sending, so a guest that never acks can be
    /// told apart from one that has not acked *yet*.
    started: Option<std::time::Instant>,
}

impl AckState {
    fn record(&mut self, sequence: u32) {
        // Monotonic: a reordered or duplicated ack cannot widen the window by
        // rewinding it.
        self.sequence = self.sequence.max(sequence);
        let now = std::time::Instant::now();
        self.last_at = Some(now);

        // Time it against when that frame went out. Everything older is now
        // implicitly acked, so it is dropped rather than left to be matched
        // against an ack that will never come.
        while let Some((seq, sent_at)) = self.in_flight.front().copied() {
            self.in_flight.pop_front();
            if seq == sequence {
                let sample = (now - sent_at).as_secs_f32() * 1000.0;
                // Smoothed, because one descheduled frame on either end is not
                // a new operating point. Weighted towards the recent, because a
                // link that has genuinely changed must not take a minute to be
                // believed.
                self.last_latency_ms = Some(match self.last_latency_ms {
                    Some(previous) => previous * 0.7 + sample * 0.3,
                    None => sample,
                });
                break;
            }
            if seq > sequence {
                // An ack for something we have no record of sending.
                self.in_flight.push_front((seq, sent_at));
                break;
            }
        }
    }

    /// Start the clock, the first time the pipeline looks at the window.
    fn begin(&mut self) {
        self.started.get_or_insert_with(std::time::Instant::now);
    }

    /// Note that a frame went out, so its ack can be timed.
    fn sent(&mut self, sequence: u32) {
        if self.in_flight.len() == ACK_SAMPLES {
            self.in_flight.pop_front();
        }
        self.in_flight.push_back((sequence, std::time::Instant::now()));
    }

    /// How long the guest actually takes to acknowledge a frame.
    ///
    /// # Why this is not the round trip
    ///
    /// The window used to be sized from the ping/pong round trip, which is the
    /// wrong quantity by exactly the guest's decode cost. A guest acks a frame
    /// once it has *decoded* it, and on a real machine that measured 9 to 12 ms
    /// a frame — three quarters of it the readback from its own GPU. So the host
    /// sized a window for acks arriving in one round trip and got them a decode
    /// later, hit the gate, stalled, burst when the backlog cleared, and hit it
    /// again. A sawtooth in the send rate, felt as stutter, with the host's own
    /// diagnosis flipping between "slow network" and "running clean" every few
    /// seconds because that is genuinely what its measurements looked like.
    ///
    /// `None` until a frame has been sent and acked, which is the only state
    /// where the round trip is still the best guess available.
    fn ack_latency_ms(&self) -> Option<f32> {
        self.last_latency_ms
    }

    /// Whether the window should be enforced at all.
    ///
    /// # A guest that has not acked *yet* is not a guest that cannot ack
    ///
    /// This used to answer "no" whenever no ack had ever arrived, so that an
    /// older guest with no ack support would not be starved after the first
    /// couple of frames. The cost was invisible and enormous: the gate stayed
    /// open for the whole of a guest's start-up, the host dumped **124 frames**
    /// into the stream before the first ack came back, and the moment it did
    /// the host was 124 frames ahead of a 3-frame window — a gap it can never
    /// close, because closing it requires sending frames it is now forbidden to
    /// send. Every session after that ran on the five-second staleness escape:
    /// one frame, then nothing, then one frame. A slideshow of stale pictures,
    /// which is exactly what it looked like from the other end.
    ///
    /// So the window is enforced from the start. A guest that has genuinely
    /// said nothing for [`ACK_STALENESS_LIMIT`] *since the session began* is the
    /// one that cannot ack, and only then does the window open.
    /// Whether this guest has ever acknowledged a single frame.
    ///
    /// The difference between a link that stopped delivering and one that has
    /// not started. Both look identical to [`Self::is_reporting`] after the
    /// staleness limit, and they call for opposite responses: a stream that was
    /// flowing and stopped is evidence about the link, while a session still
    /// establishing is no evidence at all.
    fn has_ever_acked(&self) -> bool {
        self.last_at.is_some()
    }

    fn is_reporting(&self) -> bool {
        match self.last_at {
            Some(at) => at.elapsed() < ACK_STALENESS_LIMIT,
            None => self
                .started
                .is_some_and(|at| at.elapsed() < ACK_STALENESS_LIMIT),
        }
    }
}

type SharedAck = Arc<std::sync::Mutex<AckState>>;

/// The guest's viewport, as it last reported it.
///
/// Read by the capture thread, written by the reader when a `Config` arrives.
/// Shared rather than sent once at start-up because a window that is resized or
/// put fullscreen mid-session changes the answer, and streaming for the size it
/// used to be is the same waste in the other direction.
type SharedViewport = Arc<std::sync::Mutex<Option<(u16, u16)>>>;

/// The display a guest has asked to watch, as a device name.
///
/// Written by the reader task when a `WatchDisplay` arrives, read by the
/// capture thread on its next tick. The same shape as [`SharedViewport`] and
/// [`CapturePlan`] and for the same reason: the capture thread owns the
/// capturer, so everything that reconfigures it arrives as state to notice
/// rather than as a call.
type WantedDisplay = Arc<std::sync::Mutex<super::screen_capture::DisplayTarget>>;

/// Which screens a session may see, as decided when it was allowed in.
///
/// Shared because two places need it: the reader, which refuses a request for a
/// screen outside it, and the session start, which only offers what is in it.
type AllowedDisplays = Arc<std::sync::Mutex<super::protocol::DisplayGrant>>;

/// The host's settings, shared with every running session.
///
/// Read by the encode thread once per evaluation window so a change made in the
/// settings window reaches a session already in progress. See
/// [`spawn_capture_pipeline`].
pub type LiveConfig = Arc<std::sync::Mutex<SessionConfig>>;

/// Set when the guest has asked for a frame that stands alone.
///
/// The reader task learns about it and the encode thread is the only place that
/// can act on it, so it crosses between them as a flag rather than a call.
type KeyframeWanted = Arc<std::sync::atomic::AtomicBool>;

/// How long an ack stays meaningful.
///
/// Past this the guest is treated as not reporting, so the window opens rather
/// than stalling the stream on a peer whose acks are lost or whose build does
/// not send them.
const ACK_STALENESS_LIMIT: Duration = Duration::from_secs(5);

/// Frames whose send time is remembered, for timing their acks.
///
/// Only needs to cover the window itself — anything older than the largest
/// window we would ever allow has already been acked or given up on.
const ACK_SAMPLES: usize = 32;

/// How far ahead of the round trip the host may run, in milliseconds.
///
/// # Why milliseconds and not frames
///
/// It was two *frames*, and a frame is not a fixed amount of time. At 60 fps
/// two frames is 33 ms; at the 8 fps a struggling stream falls back to, it is
/// 250 ms — so the slack quietly grew by a factor of seven exactly when latency
/// mattered most. A session measured at 433 ms end to end over a 102 ms link had
/// most of the difference sitting in this window.
///
/// The frames genuinely in flight still scale with the round trip; see
/// [`send_window`]. This is the queue on top of them, and a queue is a duration.
const SEND_WINDOW_SLACK_MS: f32 = 40.0;

/// Upper bound on the send window, whatever the round trip says.
///
/// This is the real latency guarantee: however bad the link, the host is never
/// more than this many frames ahead of what the guest has actually shown.
const SEND_WINDOW_MAX: u32 = 8;

/// How many frames the host may have outstanding before it stops encoding.
///
/// **Without this, a guest that decodes slower than the host encodes builds an
/// unbounded backlog.** QUIC is reliable and its send buffer is megabytes, so
/// the host happily queues seconds of video that the guest works through long
/// after it stopped being useful — the picture ends up minutes behind, and the
/// guest keeps drawing buffered frames even after the host has quit. Neither the
/// bounded frame channel nor the adaptive controller catches it: the channel
/// only bounds the host's *own* queue, and the controller lowers quality rather
/// than slowing down.
///
/// The window scales with the round trip because in-flight frames are mostly
/// propagation delay on a distant link, and a fixed count would cap throughput
/// at one frame per RTT.
fn send_window(ack_ms: f32, fps: u32) -> u32 {
    let frame_ms = 1000.0 / fps.max(1) as f32;
    // How many frames are in the air during one acknowledgement. Sized from the
    // *measured* time to be acked rather than the ping round trip — see
    // `AckState::ack_latency_ms` for what that difference cost.
    let in_transit = (ack_ms.max(0.0) / frame_ms).ceil() as u32;
    // At least one frame of slack, however slow the stream: without it the host
    // sends one frame per acknowledgement and the link never fills.
    let slack = ((SEND_WINDOW_SLACK_MS / frame_ms).ceil() as u32).max(1);
    in_transit.saturating_add(slack).clamp(2, SEND_WINDOW_MAX)
}

/// Which virtual display a session has taken for its own, if any.
///
/// One per session, and it never moves once set: a display can only be one
/// size, so a second session claiming the same one would take it from the first
/// and the two would fight over it for as long as both were connected.
type ClaimedDisplay = Arc<std::sync::Mutex<Option<String>>>;

/// Resize the virtual display this guest is watching to match its window.
///
/// # What this deliberately will not do
///
/// **A real monitor is never touched.** It belongs to whoever is sitting at it,
/// and resizing one because somebody connected rearranges a desk nobody asked
/// to have rearranged. Only a display the host created for this purpose is
/// fair game, which is what `virtual_display::available` answers.
///
/// **A display already claimed by another session is never taken.** The first
/// guest to watch one keeps it. A second guest who wants their own size wants a
/// second virtual display, and the host creates those deliberately.
///
/// Failures are logged and dropped. The session works either way — the picture
/// is scaled rather than 1:1, which is what every session did before this
/// existed — and ending one over a display that would not resize would be a
/// worse outcome than the thing being fixed.
fn follow_guest_size(
    session_id: u32,
    wanted: &WantedDisplay,
    size: (u16, u16),
    claimed: &ClaimedDisplay,
) {
    let watching = wanted.lock().unwrap().clone();
    let super::screen_capture::DisplayTarget::One(name) = watching else {
        // The wall, or the primary by default. Neither is one display to
        // resize, and "every screen" has no single size to give it.
        return;
    };

    let Some(screen) = super::virtual_display::available()
        .into_iter()
        .find(|screen| screen.name == name && screen.ready)
    else {
        debug!("Session #{session_id}: {name} is not a virtual display — leaving its size");
        return;
    };

    {
        let mut claimed = claimed.lock().unwrap();
        match claimed.as_deref() {
            // Already ours, so this is the guest's window changing size rather
            // than a first claim. Still worth following.
            Some(mine) if mine == name => {}
            Some(other) => {
                debug!("Session #{session_id}: already following {other}, not taking {name}");
                return;
            }
            None => {
                if let Some(taken) = super::session::claim_virtual_display(&name, session_id) {
                    debug!(
                        "Session #{session_id}: {name} already belongs to session #{taken}"
                    );
                    return;
                }
                *claimed = Some(name.clone());
            }
        }
    }

    match super::virtual_display::attach(&name, size, 60) {
        Ok(()) => info!(
            "Session #{session_id}: {name} resized to {}x{} to match the guest",
            size.0, size.1
        ),
        Err(e) => warn!("Session #{session_id}: could not resize {name}: {e:#}"),
    }
    let _ = screen;
}

/// Whether the guest's silence is evidence that the link has stopped carrying.
///
/// Three things have to be true at once, and each rules out a different way of
/// being wrong about a connection that is perfectly fine:
///
/// - **Something is outstanding.** Silence says nothing about a link that was
///   given nothing to carry. A still desktop produces no frames, so nothing
///   goes out and nothing comes back — and everything sent having been
///   acknowledged is the opposite of a link in trouble.
/// - **Something has been acknowledged before.** A session in its first seconds
///   has not been acknowledged either, and that is a stream still starting
///   rather than one that stopped.
/// - **Nothing has been acknowledged recently.** The signal itself.
///
/// Pure, because getting it wrong is invisible: the result is a resolution that
/// falls while the encoder is idle and the round trip reads three milliseconds,
/// and the only clue is a diagnosis that cannot be true.
fn acks_are_stale(outstanding: u32, has_ever_acked: bool, reporting: bool) -> bool {
    outstanding > 0 && has_ever_acked && !reporting
}

/// What to size the window against: how long an ack really takes, or the round
/// trip until one has been timed.
fn ack_or_rtt_ms(acks: &SharedAck, telemetry: &SharedTelemetry) -> f32 {
    acks.lock()
        .unwrap()
        .ack_latency_ms()
        .unwrap_or_else(|| guest_rtt(telemetry))
}

/// Round trip the guest last reported, or zero before it has said anything.
fn guest_rtt(telemetry: &SharedTelemetry) -> f32 {
    telemetry.lock().unwrap().map(|t| t.rtt_ms).unwrap_or(0.0)
}

/// What the capture thread reports back alongside its frames.
pub enum PipelineEvent {
    Frame(super::encoder::EncodedFrame),
    /// A blurred thumbnail of this host's screen, produced once per session.
    Preview(Vec<u8>),
    /// The host switched codec mid-session; the guest needs a new decoder.
    CodecChanged(String),
    /// The stream's settings or its diagnosis changed.
    Status(super::protocol::StreamStatus),
    Failed(anyhow::Error),
}

/// Rolling per-stage measurements over one evaluation window.
///
/// Medians, not means: one 200 ms stall while another program grabbed the GPU
/// should not read as a stream that cannot keep up.
#[derive(Default)]
struct Window {
    capture: Vec<f32>,
    encode: Vec<f32>,
    blocked: Vec<f32>,
    frames: u32,
}

impl Window {
    fn median(values: &mut [f32]) -> f32 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        values[values.len() / 2]
    }

    fn observe(
        &mut self,
        elapsed: Duration,
        guest: Option<super::protocol::GuestTelemetry>,
        acks_stale: bool,
    ) -> super::adaptive::Observation {
        super::adaptive::Observation {
            capture_ms: Self::median(&mut self.capture),
            encode_ms: Self::median(&mut self.encode),
            send_blocked_ms: Self::median(&mut self.blocked),
            achieved_fps: self.frames as f32 / elapsed.as_secs_f32().max(0.001),
            acks_stale,
            guest,
        }
    }

    fn clear(&mut self) {
        self.capture.clear();
        self.encode.clear();
        self.blocked.clear();
        self.frames = 0;
    }
}

/// How often the host says what it did with the last few seconds.
const TALLY_INTERVAL: Duration = Duration::from_secs(5);

/// The shortest gap between two keyframes a guest can ask the encoder for.
///
/// Long enough that one has demonstrably arrived and been tried — a keyframe
/// and the round trip back are tens of milliseconds on any link worth streaming
/// over — and short enough to be invisible to a guest asking for a good reason,
/// which asks once. See where it is read for what a guest asking for a bad
/// reason did to the host.
const KEYFRAME_FLOOR: Duration = Duration::from_millis(500);

/// What the host produced, and what became of it.
///
/// The mirror of the guest's `FrameTally`, and it exists for the same reason:
/// a stream that is not moving looks identical from the outside whether the
/// desktop never changed, the encoder skipped every frame, the guest's acks
/// held the window shut, or the link would not take the bytes. Only the host
/// can tell those four apart, and until this existed it said nothing — which
/// cost several rounds of diagnosing a still desktop as a slow network.
#[derive(Debug, Default)]
struct HostTally {
    /// Frames the capturer produced.
    captured: u32,
    /// Windows where no frame arrived from the capture thread within
    /// `EVAL_WINDOW`. **Expected on an idle screen**, where the only frames are
    /// the once-a-second refresh.
    nothing_changed: u32,
    /// Frames dropped before the encoder because the guest is too far behind.
    held_for_acks: u32,
    /// Frames the encoder produced no bytes for, to hold its bitrate.
    skipped_by_encoder: u32,
    /// Frames handed to the network.
    sent: u32,
    bytes: usize,
    /// Time spent blocked handing frames over, which is the link's own limit.
    blocked: Duration,
    /// How far the last sent frame is ahead of the last acked one.
    ahead: u32,
    /// How far ahead it is allowed to be.
    window: u32,
    acked: u32,
    sent_sequence: u32,
    /// Measured send-to-ack time, once one frame has completed the round.
    ack_latency_ms: Option<f32>,
    /// Age of the most recent ack, or `None` if the guest has never sent one.
    ack_age: Option<Duration>,
}

impl HostTally {
    fn report(&self, session_id: u32, elapsed: Duration) {
        let secs = elapsed.as_secs_f32().max(0.001);
        let per_sec = |n: u32| n as f32 / secs;
        let mut line = format!(
            "Session #{session_id}: {:.1} captured/s, {:.1} sent/s, {:.0} kbit/s",
            per_sec(self.captured),
            per_sec(self.sent),
            self.bytes as f32 * 8.0 / secs / 1000.0,
        );
        // Each of these is a different culprit, so each is named only when it
        // actually happened rather than padding every line with zeroes.
        if self.nothing_changed > 0 {
            line.push_str(&format!(
                ", {:.1}/s nothing to capture", per_sec(self.nothing_changed)
            ));
        }
        if self.held_for_acks > 0 {
            line.push_str(&format!(
                ", {:.1}/s held for acks", per_sec(self.held_for_acks)
            ));
        }
        if self.skipped_by_encoder > 0 {
            line.push_str(&format!(
                ", {:.1}/s skipped by the encoder", per_sec(self.skipped_by_encoder)
            ));
        }
        // # Blocking here is the mechanism, not a fault
        //
        // Handing a frame to the writer blocks when the link is behind, and
        // that is the backpressure the whole arrangement exists to produce. A
        // high figure on its own means the network is the limit — which is
        // information, not a bug.
        //
        // It was once 961 ms/s, and that *was* a bug: the ack window had locked
        // the guest out entirely, so the writer never drained. The distinguishing
        // symptom is frames being held or dropped alongside it, both of which
        // are on this same line.
        if self.blocked.as_millis() > 0 {
            line.push_str(&format!(
                ", waiting on the link {:.0} ms/s", self.blocked.as_secs_f32() * 1000.0 / secs
            ));
        }
        // Always shown when the gate is doing anything, because a window that
        // never reopens looks exactly like a slow link from every other angle.
        if self.held_for_acks > 0 || self.ahead > 0 {
            line.push_str(&format!(
                " [sent #{} acked #{}, {} ahead of a {} window sized on {}, last ack {}]",
                self.sent_sequence,
                self.acked,
                self.ahead,
                self.window,
                // The number the window is actually derived from. Round trip and
                // acknowledgement differ by the guest's decode, and which one is
                // in play decides whether a stalled window is the link's fault.
                match self.ack_latency_ms {
                    Some(ms) => format!("{ms:.0}ms acks"),
                    None => "the round trip".to_string(),
                },
                match self.ack_age {
                    Some(age) => format!("{:.1}s ago", age.as_secs_f32()),
                    None => "never".to_string(),
                },
            ));
        }
        info!("{line}");
    }
}

/// Run capture and encode on a dedicated OS thread.
///
/// Returns the receiving end of the event channel and the thread's handle. Drop
/// the receiver to stop the thread. See the module docs for why this cannot live
/// on the async runtime.
///
/// The adaptive controller lives here rather than in the session loop because
/// this is the only place that can see how long each stage took *and* act on it
/// without another hop.
/// Capture, encode and send this machine's audio for one session.
///
/// Returns `None` when there is nothing to send with — no capture device, no
/// Opus encoder, or a build without ffmpeg. A session without sound is a
/// working session, so none of those are errors.
#[cfg(all(target_os = "windows", db_ffmpeg))]
fn spawn_audio_pipeline(
    session_id: u32,
    out_tx: mpsc::UnboundedSender<Message>,
    exclude: &[String],
    bitrate_kbps: u32,
) -> Option<AudioPipeline> {
    use super::audio_capture::{audio_sessions, LoopbackCapture};
    use super::audio_codec::{OpusEncoder, DEFAULT_BITRATE};

    // Resolved now rather than saved: a pid is different every launch, and this
    // is the moment the named applications are most likely to be running.
    let sessions = if exclude.is_empty() { Vec::new() } else { audio_sessions() };
    let pids: Vec<u32> = sessions
        .iter()
        .filter(|s| exclude.iter().any(|name| name.eq_ignore_ascii_case(&s.name)))
        .map(|s| s.pid)
        .collect();
    for name in exclude {
        if !sessions.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
            // Worth saying: the user asked for something that did not happen,
            // and silence means they find out by hearing the excluded app.
            info!(
                "Session #{session_id}: {name} has no audio session right now — it is not being excluded"
            );
        }
    }

    let started = LoopbackCapture::start_excluding_many(&pids);
    let (capture, chunks) = match started {
        Ok(pair) => pair,
        Err(e) => {
            info!("Session #{session_id}: no system audio to send ({e})");
            return None;
        }
    };
    let format = capture.format();
    // Opus takes 1 or 2 channels. A surround endpoint is downmixed rather than
    // refused, because streaming the front pair beats streaming nothing.
    let channels = format.channels.min(2);
    let bitrate = if bitrate_kbps == 0 { DEFAULT_BITRATE } else { bitrate_kbps * 1000 };
    let mut encoder = match OpusEncoder::new(format.sample_rate, channels, bitrate) {
        Ok(encoder) => encoder,
        Err(e) => {
            warn!("Session #{session_id}: no Opus encoder ({e}) — sending no audio");
            return None;
        }
    };

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(format!("audio-send-{session_id}"))
        .spawn(move || {
            // Held for the life of the thread: dropping it stops the capture.
            let _capture = capture;
            let src = format.channels as usize;
            let dst = channels as usize;
            let mut mono = Vec::new();

            while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok(chunk) = chunks.recv_timeout(std::time::Duration::from_millis(500)) else {
                    continue;
                };
                let samples = if src == dst {
                    &chunk.samples[..]
                } else {
                    // Take the first `dst` channels of each frame. Crude beside
                    // a proper downmix matrix, and correct for the front pair,
                    // which is what a desktop actually puts there.
                    mono.clear();
                    for frame in chunk.samples.chunks_exact(src) {
                        mono.extend_from_slice(&frame[..dst.min(frame.len())]);
                    }
                    &mono[..]
                };

                let packets = match encoder.encode(samples) {
                    Ok(packets) => packets.to_vec(),
                    Err(e) => {
                        warn!("Session #{session_id}: audio encode failed ({e})");
                        break;
                    }
                };
                // Empty is routine, not a failure: the buffer did not complete
                // an Opus frame and the remainder is held for the next one.
                if packets.is_empty() {
                    continue;
                }
                let frames = (samples.len() / dst) as u32;
                let captured_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let message = Message::Audio(super::protocol::AudioMessage {
                    packets,
                    first_frame: chunk.position,
                    captured_ms,
                    frames,
                    sample_rate: format.sample_rate,
                    channels: channels as u8,
                });
                // The guest went away; the session loop will notice too.
                if out_tx.send(message).is_err() {
                    break;
                }
            }
        })
        .ok()?;

    Some(AudioPipeline { stop, thread: Some(thread) })
}

#[cfg(not(all(target_os = "windows", db_ffmpeg)))]
fn spawn_audio_pipeline(
    _session_id: u32,
    _out_tx: mpsc::UnboundedSender<Message>,
    _exclude: &[String],
    _bitrate_kbps: u32,
) -> Option<AudioPipeline> {
    None
}

/// Handle on the audio thread, so a session can stop it.
pub struct AudioPipeline {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPipeline {
    fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_capture_pipeline(
    session_id: u32,
    capturer: Box<dyn super::screen_capture::ScreenCaptureBackend>,
    encoder: Box<dyn super::encoder::VideoEncoder>,
    config: SessionConfig,
    live_config: LiveConfig,
    telemetry: SharedTelemetry,
    acks: SharedAck,
    viewport: SharedViewport,
    keyframe_wanted: KeyframeWanted,
    cursor_tx: mpsc::UnboundedSender<super::cursor::CursorUpdate>,
    wanted_display: WantedDisplay,
    allowed_displays: AllowedDisplays,
    injector: Arc<dyn super::input_inject::InputInjector>,
    // For the messages the capture thread has to send that are not frames —
    // what the shared window is doing, and which shared windows are still open.
    // See `SharedWindowWatch` and `Offer`.
    out_tx: mpsc::UnboundedSender<Message>,
) -> (mpsc::Receiver<PipelineEvent>, std::thread::JoinHandle<()>) {
    let mut capturer = capturer;
    // Settle the pixel format before either thread starts. If the encoder wants
    // NV12 and the capture backend can produce it on the GPU, that removes the
    // CPU colour conversion *and* two thirds of the readback — see
    // `capture_nv12`. Neither side can change its mind afterwards.
    let format = super::screen_capture::negotiate_format(&mut *capturer, encoder.preferred_format());
    if format != super::screen_capture::PixelFormat::Bgra {
        info!("Session #{session_id}: capturing directly as {format:?}");
    }
    // Settled here for the same reason, and it is not advisory: an encoder
    // handed a size off its grid can fail in ways that look like anything but a
    // size — see `VideoEncoder::size_granularity`.
    let granularity = encoder.size_granularity();
    if granularity != (2, 2) {
        info!(
            "Session #{session_id}: this encoder takes frames in multiples of {}x{}",
            granularity.0, granularity.1
        );
    }
    capturer.set_size_granularity(granularity);

    let (tx, rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
    // Depth 1: one frame captured and waiting while the previous one encodes.
    // More would only add staleness — a frame queued behind two others is
    // already older than the next capture will be.
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<ScreenFrame>(1);

    // What the capture thread should be doing, written by the encode thread
    // when the controller changes its mind.
    let capture_plan = Arc::new(std::sync::Mutex::new((
        config.capture.quality,
        config.capture.fps.max(1),
    )));
    let capture_thread = spawn_capture_thread(
        session_id,
        capturer,
        raw_tx,
        Arc::clone(&capture_plan),
        viewport,
        cursor_tx,
        wanted_display,
        allowed_displays,
        config.capture.clone(),
        format,
        granularity,
        Arc::clone(&injector),
        out_tx,
    );

    let handle = std::thread::Builder::new()
        .name(format!("tain-encode-{session_id}"))
        .spawn(move || {
            let mut encoder = encoder;
            let mut encode_config = config.encode.clone();
            let native = config.capture.native_size();
            let mut controller = super::adaptive::AdaptiveController::new(
                config.priority,
                config.ceiling_plan(native),
                native,
            );

            let mut window = Window::default();
            let mut window_started = std::time::Instant::now();
            let mut sent_sequence: u32 = 0;
            let mut dropped_for_backpressure: u32 = 0;
            let mut tally = HostTally::default();
            let mut tallied_at = std::time::Instant::now();
            // The last status the guest was told, so an unchanged one is not
            // resent every window.
            let mut last_status: Option<super::protocol::StreamStatus> = None;
            // What the shared settings looked like the last time they were
            // acted on, so an unchanged config costs one comparison.
            let mut applied_config = config.clone();
            // Frames a second actually leaving here, from the last completed
            // window. The window below is a duration, and this is what turns it
            // into a number of frames.
            // One picture per session, from the first frame that arrives.
            let mut sent_preview = false;
            // When the last keyframe went out, so a guest cannot ask for them
            // faster than they are worth sending. See where this is read.
            let mut keyframe_sent_at: Option<std::time::Instant> = None;

            loop {
                // Blocks until the capture thread has something, which is also
                // how this thread learns the session ended: the sender drops.
                let captured = match raw_rx.recv_timeout(EVAL_WINDOW) {
                    Ok(frame) => Some(frame),
                    // An idle desktop produces nothing, and the controller still
                    // has to run — a session that goes quiet must not stop
                    // adapting. This is the tick that keeps it alive.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };

                // Stop encoding once the guest is too far behind. Dropping here
                // — before the encoder — is the only safe place: no inter-frame
                // state has been created yet, so the next frame we do encode
                // still references one the guest has.
                // # Sized from the rate being aimed for, not the one achieved
                //
                // This used to use the observed rate, to keep the window a
                // consistent *duration* rather than a frame count. The
                // reasoning was sound and the result was a ratchet.
                //
                // Throughput through a window is `window / ack_latency`, and
                // the window is `ack_latency / frame_interval`. Compute the
                // frame interval from the observed rate and the two equations
                // close on themselves: a stream at 10 fps has a 100 ms frame
                // interval, so with a 150 ms ack it gets a window of two, which
                // admits about 13 fps, which keeps it at 10 fps, which keeps
                // the window at two. A stable point far below what the link can
                // carry, and nothing in the loop can climb out of it — the
                // window shrank *because* the rate fell, and the rate cannot
                // rise until the window does.
                //
                // Measured on a link with 30 Mbit/s upstream and a 40 ms round
                // trip: 10.7 frames a second at 1.2 Mbit/s, with the host
                // reporting a slow network throughout.
                //
                // Sized from the plan instead, the window is the
                // bandwidth-delay product of the rate being attempted, which is
                // what a window is for. The stream can then reach the plan, and
                // if the link genuinely cannot carry it the blocked-send
                // measurement says so and the controller lowers the plan — one
                // feedback loop, driven by measurement, instead of two driving
                // each other downwards.
                let allowed =
                    send_window(ack_or_rtt_ms(&acks, &telemetry), controller.plan().fps);
                let too_far_ahead = {
                    let mut state = acks.lock().unwrap();
                    state.begin();
                    // Recorded whether or not the gate closes: "how far ahead
                    // are we, and of what" is the question every stalled
                    // session has come down to, and inferring it from frame
                    // counts has been wrong more often than right.
                    tally.ahead = sent_sequence.saturating_sub(state.sequence);
                    tally.window = allowed;
                    tally.acked = state.sequence;
                    tally.sent_sequence = sent_sequence;
                    tally.ack_age = state.last_at.map(|at| at.elapsed());
                    tally.ack_latency_ms = state.ack_latency_ms();
                    state.is_reporting() && tally.ahead >= allowed
                };
                let captured = match captured {
                    Some(_) if too_far_ahead => {
                        dropped_for_backpressure += 1;
                        tally.held_for_acks += 1;
                        None
                    }
                    other => other,
                };
                match captured {
                    Some(_) => tally.captured += 1,
                    // No frame this tick. On a still desktop that is the whole
                    // story, and the reason to count it separately: a host with
                    // nothing to send looks exactly like one that cannot send.
                    None if !too_far_ahead => tally.nothing_changed += 1,
                    None => {}
                }

                if let Some(captured) = captured {
                    // One picture of this screen, from the first real frame, so
                    // the guest can label its card for this host later. Done
                    // here because this is where pixels exist in a known format
                    // whichever way the capture path was configured.
                    if !sent_preview {
                        sent_preview = true;
                        // The wallpaper first. It says *which machine this is*,
                        // which is what the card is for; a frame says what the
                        // machine happened to be showing when someone connected,
                        // which changes every session and is somebody's work.
                        // A screen frame is the fallback for a desktop with no
                        // wallpaper to read.
                        let picture = super::wallpaper::preview().or_else(|| {
                            super::preview::from_frame(
                                &captured.data,
                                captured.width as usize,
                                captured.height as usize,
                                captured.format,
                            )
                        });
                        if let Some(picture) = picture {
                            let _ = tx.blocking_send(PipelineEvent::Preview(picture));
                        }
                    }

                    // Honoured here rather than where it arrives: this is the
                    // only thread that owns the encoder.
                    //
                    // # Why a guest cannot ask faster than this
                    //
                    // A guest asks for a keyframe when a frame fails to decode,
                    // and a guest whose decoder is genuinely stuck fails on
                    // every frame — so it asks on every frame. Measured at
                    // three a second, and on some encoders a keyframe means
                    // rebuilding the transform, which is hundreds of
                    // milliseconds of the capture thread each time.
                    //
                    // The host then went from serving a guest with a broken
                    // picture to having no picture to serve: throughput fell to
                    // a ninth, and AMD's AV1 transform — rebuilt six times in
                    // under two seconds — stalled and took the session with it.
                    // One end's fault became both ends' outage.
                    //
                    // Nothing is lost by refusing. A keyframe sent a fifth of a
                    // second ago is still arriving; if it did not help, another
                    // one will not either, and the guest's problem is somewhere
                    // this cannot reach. The request is dropped rather than
                    // deferred for the same reason — it asks for a keyframe
                    // *now*, and one already is.
                    if keyframe_wanted.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        match keyframe_sent_at {
                            Some(at) if at.elapsed() < KEYFRAME_FLOOR => {
                                trace!(
                                    "Session #{session_id}: a keyframe went out {:?} ago — \
                                     not rebuilding the encoder for another",
                                    at.elapsed()
                                );
                            }
                            _ => {
                                encoder.request_keyframe();
                                keyframe_sent_at = Some(std::time::Instant::now());
                            }
                        }
                    }
                    let encode_started = std::time::Instant::now();
                    let encoded = match encoder.encode_frame(
                        &captured.data,
                        captured.width,
                        captured.height,
                        captured.format,
                    ) {
                        Ok(encoded) => encoded,
                        Err(e) => {
                            let _ = tx.blocking_send(PipelineEvent::Failed(e));
                            break;
                        }
                    };
                    let encoded_at = std::time::Instant::now();

                    // The backend's own figure, not wall time — see
                    // `ScreenFrame::cost` for why that distinction decides
                    // whether an idle screen accuses itself of being slow.
                    window.capture.push(captured.cost.as_secs_f32() * 1000.0);
                    window.encode.push((encoded_at - encode_started).as_secs_f32() * 1000.0);

                    // An encoder that skipped this frame to hold its bitrate has
                    // nothing to say; sending an empty payload would look like a
                    // decode failure on the guest.
                    if encoded.data.is_empty() {
                        // The encoder held this frame back to stay inside its
                        // bitrate. Distinct from a frame that never existed.
                        tally.skipped_by_encoder += 1;
                    } else {
                        tally.sent += 1;
                        tally.bytes += encoded.data.len();
                        sent_sequence = encoded.sequence_number;
                        // Timed from here, which is the moment it is handed to
                        // the transport — the ack that comes back measures
                        // everything after this point, including the guest's
                        // decode.
                        acks.lock().unwrap().sent(sent_sequence);
                        // Blocks when the network is behind, which is the
                        // backpressure this whole arrangement exists to get —
                        // and timing the block is the cleanest signal there is
                        // that the link, not the CPU, is the limit. Failure
                        // means the session ended and the receiver was dropped.
                        if tx.blocking_send(PipelineEvent::Frame(encoded)).is_err() {
                            break;
                        }
                        let blocked = std::time::Instant::now() - encoded_at;
                        tally.blocked += blocked;
                        window.blocked.push(blocked.as_secs_f32() * 1000.0);
                        window.frames += 1;
                    }
                }

                if tallied_at.elapsed() >= TALLY_INTERVAL {
                    tally.report(session_id, tallied_at.elapsed());
                    tally = HostTally::default();
                    tallied_at = std::time::Instant::now();
                }

                let elapsed = window_started.elapsed();
                if elapsed < EVAL_WINDOW {
                    continue;
                }

                let guest = *telemetry.lock().unwrap();
                // # Stopped, not "not started yet"
                //
                // The same silence the flow-control window uses to decide a
                // guest cannot ack — see `AckState::is_reporting`. There it
                // opens the window so frames keep flowing; here it is what
                // tells the controller they are flowing nowhere.
                //
                // But only once there has been something to stop. A session in
                // its first seconds has not been acknowledged yet either, and
                // reading that as a failing link is how a stream on a
                // five-millisecond connection was taken down to 640x360 at
                // 8 fps before its first frame was ever confirmed — and then
                // spent seventy seconds climbing back out.
                //
                // # And only when there is something to acknowledge
                //
                // Silence is evidence about the link only if the link was given
                // something to carry. A still desktop produces no frames — the
                // capture path says so itself, counting "nothing changed"
                // separately from "nothing sent" — so nothing goes out, nothing
                // comes back, and five seconds later this called a healthy
                // connection stale.
                //
                // Reported as a resolution that drops while the encoder has
                // headroom and the round trip reads three milliseconds, which is
                // exactly what it looks like from outside: the one diagnosis
                // that cannot be true is the one being made. Worse on precisely
                // the screens this is most wanted for — a virtual display or a
                // dummy plug showing a still picture sends nothing for minutes.
                //
                // So a frame has to be outstanding. Everything sent having been
                // acknowledged is the *opposite* of a link in trouble.
                let acks_stale = {
                    let state = acks.lock().unwrap();
                    acks_are_stale(
                        sent_sequence.saturating_sub(state.sequence),
                        state.has_ever_acked(),
                        state.is_reporting(),
                    )
                };
                let observation = window.observe(elapsed, guest, acks_stale);
                window.clear();
                window_started = std::time::Instant::now();

                if dropped_for_backpressure > 0 {
                    debug!(
                        "Session #{session_id}: held back {dropped_for_backpressure} frames \
                         to stay within {} of the guest",
                        send_window(ack_or_rtt_ms(&acks, &telemetry), controller.plan().fps),
                    );
                    dropped_for_backpressure = 0;
                }

                if let Some(plan) = controller.observe(&observation) {
                    info!(
                        "Session #{}: adapting to {} — {} at {}fps, {}kbps",
                        session_id,
                        controller
                            .bottlenecks()
                            .iter()
                            .map(|b| b.label())
                            .collect::<Vec<_>>()
                            .join(", "),
                        plan.scale.label(),
                        plan.fps,
                        plan.bitrate_kbps,
                    );

                    // The capture thread owns the capturer, so the plan reaches
                    // it through here rather than by calling into it directly.
                    *capture_plan.lock().unwrap() = (plan.scale, plan.fps);
                    encode_config.fps = plan.fps;
                    encode_config.target_bitrate_kbps = plan.bitrate_kbps;
                    if let Err(e) = encoder.reconfigure(&encode_config) {
                        warn!("Session #{}: could not apply the new plan: {e}", session_id);
                    }
                }

                // # Sent on every change, not only when the plan moves
                //
                // This used to sit behind the `else { continue }` above, so the
                // guest was told what the stream was doing **only at the moment
                // the controller changed it**. Two consequences, both reported:
                //
                // A healthy session never changes its plan, so it never sent
                // one at all, and the guest sat on "the host has not reported
                // its settings yet" for the whole session. That is the
                // negotiation that looked broken — the host knew, and simply
                // never said.
                //
                // And the bottleneck chips could not clear. They were set at the
                // last plan change and then frozen, so a session showed "Slow
                // network" long after the link recovered, which is worse than
                // showing nothing: it is a diagnosis pointing at the wrong thing.
                // # Settings that reach a session already running
                //
                // Everything the controller owns — frame rate, resolution
                // ceiling, bitrate, and which axis it gives up first — is
                // already adjusted mid-session several times a minute, so
                // following a change made in the settings window costs nothing
                // beyond noticing it. The encoder's own knobs are handled by
                // `reconfigure`, which rebuilds only when it has to.
                //
                // The codec is the one thing that cannot simply change: the
                // guest built a decoder from what `SessionStart` said, and
                // switching under it produces a stream it cannot read. So that
                // case rebuilds the encoder *and* tells the guest, which
                // rebuilds its decoder and asks for a keyframe.
                let wanted = live_config.lock().unwrap().clone();
                if wanted != applied_config {
                    let before = applied_config.encode.effective_codec();
                    let after = wanted.encode.effective_codec();

                    controller.set_ceiling(wanted.ceiling_plan(native));
                    controller.set_priority(wanted.priority);
                    encode_config = wanted.encode.clone();
                    encode_config.fps = controller.plan().fps;
                    encode_config.target_bitrate_kbps = controller.plan().bitrate_kbps;

                    if before != after {
                        info!("Session #{session_id}: switching from {before} to {after}");
                        match super::encoder::create_encoder(encode_config.clone()) {
                            Ok(replacement) => {
                                encoder = replacement;
                                // Told before the first frame of the new codec
                                // arrives, so the guest is never handed a
                                // bitstream it has no decoder for.
                                let _ = tx.blocking_send(PipelineEvent::CodecChanged(
                                    after.to_string(),
                                ));
                            }
                            Err(e) => {
                                warn!("Session #{session_id}: cannot switch to {after}: {e:#}");
                                encode_config.codec = before;
                            }
                        }
                    } else if let Err(e) = encoder.reconfigure(&encode_config) {
                        warn!("Session #{session_id}: could not apply new settings: {e:#}");
                    }

                    *capture_plan.lock().unwrap() =
                        (controller.plan().scale, controller.plan().fps);
                    applied_config = wanted;
                }

                let plan = controller.plan();
                let status = super::protocol::StreamStatus {
                    session_id,
                    bottlenecks: controller.bottlenecks().bits(),
                    fps: plan.fps,
                    bitrate_kbps: plan.bitrate_kbps,
                    degraded: controller.is_degraded(),
                };
                if last_status != Some(status) {
                    last_status = Some(status);
                    if tx.blocking_send(PipelineEvent::Status(status)).is_err() {
                        break;
                    }
                }
            }
            debug!("Session #{}: encode pipeline stopped", session_id);
            // Dropping the receiver is what stops the capture thread: its next
            // send fails and it returns.
            drop(raw_rx);
            let _ = capture_thread.join();
        })
        .expect("failed to spawn encode thread");

    (rx, handle)
}

/// What the capture thread is currently asked to produce: scale, and rate.
type CapturePlan = Arc<std::sync::Mutex<(QualityScale, u32)>>;

/// What a session is showing, said in the guest's terms.
///
/// The two vocabularies differ: the host captures a [`DisplayTarget`], which
/// names a device, and the guest picks a [`DisplayChoice`], which names an id
/// from the list it was sent. This is the translation, and it exists because
/// every "the request could not be honoured" path has to answer the same
/// question — what *is* it showing, then.
fn showing_now(
    current: &super::screen_capture::DisplayTarget,
    available: &[super::protocol::DisplayInfo],
) -> super::protocol::DisplayChoice {
    use super::protocol::DisplayChoice;
    use super::screen_capture::DisplayTarget;
    match current {
        // Both composites answer the same way: "everything this session may
        // see, at once". Which of the two it is follows from the grant, and the
        // guest already knows that.
        DisplayTarget::All | DisplayTarget::Windows(_) => DisplayChoice::All,
        DisplayTarget::Window(id) => DisplayChoice::Window(*id),
        DisplayTarget::One(name) => {
            DisplayChoice::One(available.iter().find(|d| &d.name == name).map_or(0, |d| d.id))
        }
        DisplayTarget::Primary => {
            DisplayChoice::One(available.iter().find(|d| d.primary).map_or(0, |d| d.id))
        }
    }
}

/// How often a shared window that is away is asked whether it is back.
///
/// Four times a second. Fast enough that restoring a window does not feel like
/// it stuck, slow enough that a session parked on a minimised window costs
/// nothing — which it otherwise would, since there is no frame to wait on and
/// the loop would spin.
const WINDOW_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// What the capture loop should do about the shared window this tick.
enum WindowStep {
    /// It is doing what it was doing.
    Unchanged,
    /// It changed, and the guest has not been told.
    Announce(super::protocol::SharedWindow),
}

/// Follows the window a session is currently showing.
///
/// Only exists for a session that shares windows; a screen session has `None`
/// and never pays for any of this. See [`super::window_capture`] for why a
/// window needs watching where a monitor does not.
///
/// One at a time, even when several were granted: a session streams one window,
/// and the others are not being captured and have nothing to report. Switching
/// replaces the watch — see [`Self::switched_to`].
struct SharedWindowWatch {
    id: u64,
    /// The last state read, which is what "changed" is measured against — the
    /// title moves on its own as a program is used, and a re-announced
    /// identical state would be noise on the wire.
    told: super::protocol::SharedWindow,
    /// Whether the guest has actually been sent what [`Self::told`] holds.
    ///
    /// False at the start of a session, where `SessionStart` carried it, and
    /// true after a switch, where nothing has. Without the distinction a guest
    /// that switched between two windows in the same state would be told
    /// nothing and would go on naming the window it left.
    owed: bool,
    /// Set when the window comes back, cleared once a capture exists again.
    restored: bool,
}

impl SharedWindowWatch {
    /// The watch a session starts with. `SessionStart` has already said what
    /// this window is doing, so nothing is owed until the answer moves.
    fn for_target(target: &super::screen_capture::DisplayTarget) -> Option<Self> {
        Self::watching(target, false)
    }

    /// The watch that replaces it when the guest picks a different window.
    ///
    /// Owes an announcement from the outset: the guest asked for a window and
    /// has been told nothing about it, and "the same state as the last one" is
    /// still news when it is a different window.
    fn switched_to(target: &super::screen_capture::DisplayTarget) -> Option<Self> {
        Self::watching(target, true)
    }

    fn watching(
        target: &super::screen_capture::DisplayTarget,
        owed: bool,
    ) -> Option<Self> {
        let super::screen_capture::DisplayTarget::Window(id) = target else {
            return None;
        };
        Some(Self {
            id: *id,
            told: super::window_capture::describe(*id, None),
            owed,
            restored: false,
        })
    }

    /// Whether frames can be expected.
    fn live(&self) -> bool {
        self.told.state == super::protocol::SharedWindowState::Showing
    }

    /// Whether the capture has to be built again before frames will come.
    fn needs_rebuild(&self) -> bool {
        self.restored
    }

    fn rebuilt(&mut self) {
        self.restored = false;
    }

    /// Ask the window what it is doing, and say so if the answer moved.
    fn poll(&mut self) -> WindowStep {
        let now = super::window_capture::describe(self.id, Some(&self.told));
        self.settle(now)
    }

    /// Apply a fresh reading. Split from [`Self::poll`] so the decision can be
    /// tested without a real window to minimise.
    fn settle(&mut self, now: super::protocol::SharedWindow) -> WindowStep {
        if now == self.told && !self.owed {
            return WindowStep::Unchanged;
        }
        self.owed = false;
        self.restored |= came_back(&self.told, &now);
        self.told = now.clone();
        WindowStep::Announce(now)
    }
}

/// Whether a window has just come back from being away.
///
/// The one transition the capture loop has to *act* on rather than merely
/// report: the capturer was dropped while the window was gone, so there is
/// nothing to take frames from until a new one is built. Every other change —
/// a retitle, going away, closing — only needs telling the guest about.
fn came_back(
    told: &super::protocol::SharedWindow,
    now: &super::protocol::SharedWindow,
) -> bool {
    use super::protocol::SharedWindowState::Showing;
    told.state != Showing && now.state == Showing
}

/// Which of a session's granted windows are open, and telling the guest when
/// that changes.
///
/// # Why the capture loop owns this
///
/// It is the only part of a session that already asks, every tick, what the
/// shared windows are doing — the composite is rebuilt from that answer and the
/// pointer is remapped from it. Anywhere else would be a second poller of the
/// same handles, and the two would disagree about when a window went.
///
/// What it buys the guest is a switcher that means something: a window that has
/// closed loses its button rather than keeping one that reports it closed
/// forever, and a session whose windows have *all* gone can say so instead of
/// showing a frozen picture. See [`super::protocol::SharedChanged`].
struct Offer {
    /// Every window the session was granted, open or not.
    granted: Vec<u64>,
    /// The ones open the last time the guest was told, and what each was doing.
    ///
    /// # Why the state and not only the list
    ///
    /// A window being minimised does not take it off the offer — it is still
    /// granted and can come back — so a list of ids alone does not move when
    /// one goes away. But the guest cannot tell a composite of minimised
    /// windows from a working one without knowing, and what it showed was the
    /// last frame, frozen, with nothing saying why. Reported as exactly that.
    ///
    /// The *title* is deliberately not compared. A browser retitles on every
    /// navigation and this is asked sixty times a second; the window being
    /// watched already carries its live title on its own message.
    told: Vec<(u64, super::protocol::SharedWindowState)>,
}

impl Offer {
    /// `None` for a session that shares screens, which has none of this to do.
    fn for_grant(allowed: &super::protocol::DisplayGrant) -> Option<Self> {
        let granted = allowed.windows().to_vec();
        if granted.is_empty() {
            return None;
        }
        // Seeded with what `SessionStart` already said, so an offer that has
        // not changed is not re-announced on the first tick.
        let told = Self::read(&granted).iter().map(|w| (w.id, w.state)).collect();
        Some(Self { granted, told })
    }

    fn read(granted: &[u64]) -> Vec<super::protocol::SharedWindow> {
        granted.iter().filter_map(|id| super::window_capture::on_offer(*id)).collect()
    }

    /// The offer as it stands now, if it has changed since the guest was told.
    fn moved(&mut self) -> Option<Vec<super::protocol::SharedWindow>> {
        let open = Self::read(&self.granted);
        let now: Vec<(u64, super::protocol::SharedWindowState)> =
            open.iter().map(|w| (w.id, w.state)).collect();
        if now == self.told {
            return None;
        }
        self.told = now;
        Some(open)
    }
}

/// Tell the guest what is now being streamed.
///
/// # Why the capture thread says this and the reader does not
///
/// The reader decides whether a request is allowed, which is immediate, and the
/// capture thread decides when it takes effect, which is not: building a
/// composite of several windows takes the better part of a second, and the old
/// view keeps being sent until it is ready.
///
/// Announced from the reader, the message described a view whose *previous*
/// pictures were still arriving — so a guest waiting on the new one stopped
/// waiting at the first frame after it, which was a frame of what it had just
/// left. Reported as the loading indicator going away too early and showing a
/// stale window.
fn announce(
    out_tx: &mpsc::UnboundedSender<Message>,
    showing: &super::screen_capture::DisplayTarget,
    why: Option<String>,
) -> Result<(), ()> {
    // Enumerated here rather than carried, because this runs on a switch and
    // not per frame, and a display list a second old could name a monitor that
    // has since been unplugged.
    let available = super::screen_capture::displays();
    // The error is discarded rather than returned: a closed channel means the
    // session is over, which is all the caller does anything about, and the
    // undeliverable `Message` inside it is large enough that carrying it back
    // is worth avoiding.
    out_tx
        .send(Message::DisplayChanged(super::protocol::DisplayChanged {
            showing: showing_now(showing, &available),
            why,
        }))
        .map_err(|_| ())
}

/// Whether the capture loop should sit still rather than go round again.
///
/// # Why this is a function and not two lines in the loop
///
/// It was two lines in the loop, and they were wrong in a way that took a bug
/// report to find. A shared window that is minimised or closed has nothing to
/// give, so the loop parks on a stub and sleeps instead of spinning — which is
/// right, and which it did by skipping straight back to the top.
///
/// The display switch is *below* that point. So a guest that closed one of
/// several shared windows, looked at the tab that said so, and then asked for
/// the composite again was held there for the rest of the session by a watch on
/// a window that had gone: the picture froze and every further switch was
/// ignored. Reported as "stopped reacting", which is exactly what it was.
///
/// Parking is only correct while nobody is waiting for something else.
fn should_park(
    live: bool,
    asked: &super::screen_capture::DisplayTarget,
    applied: &super::screen_capture::DisplayTarget,
) -> bool {
    !live && asked == applied
}

/// Grab frames at the requested rate and hand them to the encoder.
///
/// Separate from encoding so the two **overlap**. They used to run in sequence
/// on one thread, which meant the frame rate was bounded by their sum: measured
/// at 5.1 ms capture plus 5.4 ms encode, or 10.5 ms, against an 8.3 ms budget at
/// 120 fps. Overlapping bounds it by the slower of the two instead. Per-frame
/// latency is unchanged — a frame still waits for its own capture and its own
/// encode — so this buys throughput without costing responsiveness.
/// Allowed rather than grouped, as on `run_reader` above: every one of these is
/// a distinct thing the capture thread needs and a struct would only be a name
/// for "the arguments", read at one call site.
#[allow(clippy::too_many_arguments)]
fn spawn_capture_thread(
    session_id: u32,
    capturer: Box<dyn super::screen_capture::ScreenCaptureBackend>,
    tx: std::sync::mpsc::SyncSender<ScreenFrame>,
    plan: CapturePlan,
    viewport: SharedViewport,
    cursor_tx: mpsc::UnboundedSender<super::cursor::CursorUpdate>,
    wanted_display: WantedDisplay,
    allowed_displays: AllowedDisplays,
    capture_config: super::screen_capture::CaptureConfig,
    format: super::screen_capture::PixelFormat,
    granularity: (u16, u16),
    injector: Arc<dyn super::input_inject::InputInjector>,
    out_tx: mpsc::UnboundedSender<Message>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("tain-capture-{session_id}"))
        .spawn(move || {
            let mut capturer = capturer;
            let mut applied_scale = plan.lock().unwrap().0;
            let mut applied_ceiling: Option<(u16, u16)> = None;
            let mut applied_display = capture_config.display.clone();
            let mut next_frame_at = std::time::Instant::now();
            let (mut produced, mut unchanged, mut dropped) = (0u32, 0u32, 0u32);
            let mut produced_at = std::time::Instant::now();
            let mut window = SharedWindowWatch::for_target(&capture_config.display);
            // Where the shared window last was, so the pointer is only remapped
            // when it actually moves. See the block that maintains it below.
            let mut window_at: Option<super::window_capture::OnScreen> = None;
            // The same, for a composite: what the guest's coordinates were last
            // told to map through.
            let mut tiled_at: Option<Vec<super::input_inject::Tile>> = None;
            // Whether the composite has already reported having nothing left.
            // It is asked again on every tick and the answer does not change on
            // its own, so without this it is a log line per frame — which is
            // how a diagnosis gets buried.
            let mut nothing_to_compose = false;
            // Which of the granted windows are still open, so the guest's
            // switcher does not go on offering ones that have gone.
            let mut offer = Offer::for_grant(&allowed_displays.lock().unwrap().clone());

            loop {
                // # What the guest may still switch to
                //
                // Ahead of everything else, because the answer decides what the
                // rest of this tick is about — and because a window that has
                // gone should leave the guest's switcher at the same moment it
                // leaves the picture, not a tick later.
                if let Some(offer) = offer.as_mut()
                    && let Some(open) = offer.moved()
                {
                    info!(
                        "Session #{session_id}: {} of {} shared window(s) still open",
                        open.len(),
                        offer.granted.len()
                    );
                    let told = out_tx.send(Message::SharedChanged(
                        super::protocol::SharedChanged {
                            displays: Vec::new(),
                            windows: open,
                            sharing_windows: true,
                        },
                    ));
                    if told.is_err() {
                        return;
                    }
                }

                // # A shared window that is not on screen
                //
                // Minimising is ordinary and reversible, so it is not a failure
                // and must not end the session — but the compositor produces
                // nothing for a window in that state, so there is also nothing
                // to send. The capture is dropped while it is away (it would
                // only sit there producing nothing) and rebuilt when it comes
                // back, and the guest is told either way so it can say what
                // happened instead of showing a frozen picture.
                if let Some(watch) = window.as_mut() {
                    match watch.poll() {
                        WindowStep::Unchanged => {}
                        WindowStep::Announce(state) => {
                            if out_tx.send(Message::SharedWindow(state)).is_err() {
                                return;
                            }
                        }
                    }
                    // # Parking here must not swallow a request to look
                    // elsewhere
                    //
                    // There is nothing to capture and nothing to burn a core
                    // on, so the loop sleeps — but the display switch is
                    // *below* this, and skipping straight back to the top means
                    // it is never reached. A guest that closed one of several
                    // shared windows, looked at the tab that said so, and then
                    // asked for the composite again was held here for the rest
                    // of the session by a watch on a window that had gone: the
                    // stream froze and every further switch was ignored.
                    //
                    // Reported as "stopped reacting", which is exactly what it
                    // was. So the loop only parks while nothing else has been
                    // asked for.
                    let asked = wanted_display.lock().unwrap().clone();
                    if should_park(watch.live(), &asked, &applied_display) {
                        capturer = super::screen_capture::create_stub(&capture_config);
                        std::thread::sleep(WINDOW_POLL);
                        continue;
                    }
                    if watch.needs_rebuild() {
                        let mut config = capture_config.clone();
                        config.display = applied_display.clone();
                        match super::screen_capture::create_capturer(config) {
                            Ok(mut fresh) => {
                                super::screen_capture::negotiate_format(&mut *fresh, format);
                                fresh.set_size_granularity(granularity);
                                fresh.set_scale(applied_scale);
                                fresh.set_size_ceiling(applied_ceiling);
                                capturer = fresh;
                                watch.rebuilt();
                                info!("Session #{session_id}: the shared window is back");
                            }
                            Err(e) => {
                                // Restored but not yet capturable — the
                                // compositor can take a moment. Tried again on
                                // the next tick rather than failed on.
                                debug!("Session #{session_id}: window not ready yet: {e:#}");
                                std::thread::sleep(WINDOW_POLL);
                                continue;
                            }
                        }
                    }

                    // # The pointer maps through the window, not the screen
                    //
                    // A guest sends positions across the picture it was sent,
                    // and the injector turns them back into desktop pixels
                    // through the rectangle being shown. With nothing set that
                    // rectangle is the whole primary screen — so in a window
                    // session a click in the middle of the window landed in the
                    // middle of the *screen*, which is the sort of wrongness
                    // that reads as a calibration problem.
                    //
                    // Checked every tick rather than on the switch, because a
                    // window is not a monitor: it is dragged and resized while
                    // somebody is using it, and a rectangle fixed at session
                    // start is wrong the first time the window moves. The DWM
                    // read is cheap and the injector is only told when the
                    // answer changes.
                    if let super::screen_capture::DisplayTarget::Window(id) = &applied_display {
                        let now = super::window_capture::on_screen(*id);
                        if let Some(rect) = now
                            && window_at != now
                        {
                            injector.set_shown(super::input_inject::Shown {
                                x: rect.x,
                                y: rect.y,
                                width: rect.width,
                                height: rect.height,
                            });
                            window_at = now;
                        }
                    }
                }

                // # A composite of windows maps through several rectangles
                //
                // The block above is for a session showing *one* window and
                // never runs for a composite — there is no single window to
                // watch. But a grid of windows needs the same thing more
                // urgently: its picture is an arrangement this end invented out
                // of sources scattered across the desk, so there is no one
                // rectangle to map through at all.
                //
                // Asked of the capturer rather than worked out here, because
                // the capturer is what decided the layout and a second
                // implementation of that arithmetic would eventually disagree
                // with the first. Per tick for the same reason as above: the
                // grid does not move when a window does, but where a click has
                // to *land* certainly does.
                if matches!(applied_display, super::screen_capture::DisplayTarget::Windows(_)) {
                    let tiles = capturer.input_tiles();
                    if let Some(tiles) = tiles
                        && tiled_at.as_deref() != Some(&tiles[..])
                    {
                        injector.set_mapping(super::input_inject::Mapping::Tiled {
                            size: capturer.frame_size(),
                            tiles: tiles.clone(),
                        });
                        tiled_at = Some(tiles);
                    }
                }

                // # Switching display
                //
                // Rebuilt rather than reconfigured: a duplication belongs to one
                // output for its whole life, so there is nothing to retarget.
                // The old capturer is dropped first, which is what releases its
                // duplication — a display being switched *away from* has to let
                // go before anything else can take it, and on a switch back to
                // the same display that would otherwise be the second
                // duplication that DXGI refuses.
                // # A wall drawn from an arrangement that no longer exists
                //
                // The composed wall is a picture of *where the screens are*,
                // decided when it was built. Someone dragging a monitor to the
                // other side of the primary, or plugging one in, leaves it
                // drawing the old layout — and the pointer maps through that
                // layout, so clicks land somewhere nobody aimed.
                //
                // It cannot repair itself: its panels are attached to
                // particular displays. Asking for the same target again is what
                // rebuilds it, through the switch below that already knows how.
                //
                // # A composite of windows goes out of date differently
                //
                // It says so for its own reason — one of the shared windows has
                // gone away or come back — and the recovery below is written
                // entirely for screens: it names the primary display as what is
                // being shown, and remaps the pointer to the whole desk. Both
                // are wrong for a window session. The first is a lie the rest of
                // the loop then acts on; the second silently sends every click
                // to a coordinate on the desktop rather than into a window.
                //
                // So it rebuilds in place, keeping the target it has. Nothing
                // else needs to change: the pointer's tiles are maintained just
                // above and follow the new capture on the next tick.
                if capturer.layout_changed() {
                    if matches!(applied_display, super::screen_capture::DisplayTarget::Windows(_)) {
                        let mut config = capture_config.clone();
                        config.display = applied_display.clone();
                        match super::screen_capture::create_capturer(config) {
                            Ok(mut fresh) => {
                                info!(
                                    "Session #{session_id}: the shared windows have changed — \
                                     composing them again"
                                );
                                super::screen_capture::negotiate_format(&mut *fresh, format);
                                fresh.set_size_granularity(granularity);
                                fresh.set_scale(applied_scale);
                                // Cleared so the new grid is asked for its own
                                // size rather than keeping the old one's.
                                applied_ceiling = None;
                                fresh.set_size_ceiling(applied_ceiling);
                                capturer = fresh;
                                nothing_to_compose = false;
                            }
                            Err(e) => {
                                // Every shared window has gone. The old
                                // composite is kept — a last picture beats a
                                // black one — and this deliberately does *not*
                                // skip to the top of the loop: parking here
                                // would swallow the switch below, which is the
                                // same bug the watch above had. There is no
                                // spin either, because the composite it kept
                                // still waits its own tick for a frame.
                                if !nothing_to_compose {
                                    warn!(
                                        "Session #{session_id}: none of the shared windows are \
                                         open any more ({e:#})"
                                    );
                                    nothing_to_compose = true;
                                }
                            }
                        }
                    } else {
                        info!(
                            "Session #{session_id}: the displays have been rearranged — \
                             rebuilding the composed view"
                        );
                        applied_display = super::screen_capture::DisplayTarget::Primary;
                        // Cleared so the rebuilt wall is asked for its own size
                        // again rather than keeping the old arrangement's.
                        applied_ceiling = None;

                        // And the pointer, which maps through the desk's
                        // bounding box. Rebuilding the picture without this
                        // leaves clicks landing against the arrangement that has
                        // just gone — which is the half of the bug that is not
                        // visible, because the picture looks right while the
                        // pointer does not.
                        let desk = super::screen_capture::displays();
                        if !desk.is_empty() {
                            let left = desk.iter().map(|d| d.x).min().unwrap_or(0);
                            let top = desk.iter().map(|d| d.y).min().unwrap_or(0);
                            let right =
                                desk.iter().map(|d| d.x + i32::from(d.width)).max().unwrap_or(0);
                            let bottom =
                                desk.iter().map(|d| d.y + i32::from(d.height)).max().unwrap_or(0);
                            injector.set_shown(super::input_inject::Shown {
                                x: left,
                                y: top,
                                width: (right - left).clamp(0, u16::MAX as i32) as u16,
                                height: (bottom - top).clamp(0, u16::MAX as i32) as u16,
                            });
                        }
                    }
                }

                let wanted = wanted_display.lock().unwrap().clone();

                // # Switching to a window that is not on screen
                //
                // A guest granted several windows can pick one that has since
                // been minimised, and there is nothing to build a capture from
                // while it is away — `create_capturer` would fail, the switch
                // would be reverted, and the guest would be left with its
                // switcher pointing at a window it is not being shown.
                //
                // So the switch is taken anyway. The watch installed here is
                // what carries it: the guest is told the window is minimised,
                // the loop above parks on a stub, and the capture is built for
                // real the moment it comes back — which is the same path a
                // window minimised *during* a session already takes.
                if let super::screen_capture::DisplayTarget::Window(id) = &wanted
                    && wanted != applied_display
                    && !super::window_capture::state_of(*id).is_live()
                {
                    info!(
                        "Session #{session_id}: switching to window {id}, which is not on \
                         screen — waiting for it"
                    );
                    applied_display = wanted.clone();
                    window = SharedWindowWatch::switched_to(&applied_display);
                    capturer = super::screen_capture::create_stub(&capture_config);
                    // Switched, even though there will be no picture until the
                    // window comes back. The guest is waiting on this answer to
                    // stop waiting; what it gets instead of frames is the watch
                    // above saying the window is minimised, which is the useful
                    // thing to be told.
                    if announce(&out_tx, &applied_display, None).is_err() {
                        return;
                    }
                    continue;
                }

                if wanted != applied_display {
                    let mut config = capture_config.clone();
                    config.display = wanted.clone();
                    let named = match &wanted {
                        super::screen_capture::DisplayTarget::One(name) => name.clone(),
                        super::screen_capture::DisplayTarget::All => "every screen".to_string(),
                        super::screen_capture::DisplayTarget::Primary => {
                            "the primary display".to_string()
                        }
                        super::screen_capture::DisplayTarget::Window(id) => {
                            super::window_capture::title_of(*id)
                                .unwrap_or_else(|| format!("window {id}"))
                        }
                        super::screen_capture::DisplayTarget::Windows(ids) => {
                            format!("{} shared window(s)", ids.len())
                        }
                    };
                    // # Drop first only when the two overlap
                    //
                    // DXGI allows one duplication *per output*, so switching
                    // between two different screens does not conflict and the
                    // old capturer can be kept until the new one exists. That
                    // matters: a switch that fails then costs nothing, where
                    // dropping first turns any failure into a session with no
                    // capture at all.
                    //
                    // The wall contains every screen including the current one,
                    // and switching to the display already being shown is the
                    // same output twice. Both have to let go first.
                    let overlaps = matches!(wanted, super::screen_capture::DisplayTarget::All)
                        || matches!(applied_display, super::screen_capture::DisplayTarget::All)
                        || wanted == applied_display;
                    // Moved out either way, so every path below has to put
                    // something back — which is what stops a failed switch
                    // leaving the thread with no capturer.
                    let kept = if overlaps {
                        drop(capturer);
                        None
                    } else {
                        Some(capturer)
                    };
                    match super::screen_capture::create_capturer(config) {
                        Ok(mut fresh) => {
                            // The encoder settled on this at session start and
                            // cannot change its mind, so the new capturer has to
                            // be asked for the same thing.
                            super::screen_capture::negotiate_format(&mut *fresh, format);
                            fresh.set_size_granularity(granularity);
                            fresh.set_scale(applied_scale);
                            fresh.set_size_ceiling(applied_ceiling);
                            let (w, h) = fresh.frame_size();
                            info!("Session #{session_id}: now streaming {named} ({w}x{h})");
                            capturer = fresh;
                            applied_display = wanted;
                            // # The watch follows the switch
                            //
                            // It is pointed at one handle, and a session that
                            // was granted several windows can be told to show a
                            // different one at any moment. Left where it was,
                            // it would report the state of a window nobody is
                            // watching — pausing the stream because the window
                            // the guest *left* got minimised, and saying
                            // nothing when the one on screen does.
                            //
                            // Built as a *switch* rather than as a start: at
                            // session start the guest was told what it is
                            // looking at by `SessionStart`, and here nothing
                            // has told it. So the new window is announced on
                            // the next tick even when its state happens to
                            // match the state of the one just left.
                            window = SharedWindowWatch::switched_to(&applied_display);
                            // # Announced here, because here is where it became
                            // true
                            //
                            // The reader accepted the request and said nothing;
                            // this is the moment the guest is looking at
                            // something else. Sent before the first frame of it
                            // rather than after, so a guest waiting on the
                            // picture is told the wait is nearly over rather
                            // than being told by the picture arriving.
                            if announce(&out_tx, &applied_display, None).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            warn!("Session #{session_id}: could not switch to {named}: {e:#}");
                            // Do not ask again every tick for something that
                            // just failed.
                            *wanted_display.lock().unwrap() = applied_display.clone();
                            // And the guest is told, or it waits for a picture
                            // that was never going to come. The reader says
                            // nothing for an accepted request precisely so this
                            // can be the one answer, whichever way it goes.
                            if announce(&out_tx, &applied_display, Some(format!("{e}"))).is_err()
                            {
                                return;
                            }

                            if let Some(old) = kept {
                                // Never let go of, so there is nothing to
                                // recover: the session carries on showing what
                                // it was showing.
                                capturer = old;
                                continue;
                            }
                            // It was let go of, because the two shared an
                            // output. Rebuilding it is the only way to keep the
                            // session alive.
                            let mut config = capture_config.clone();
                            config.display = applied_display.clone();
                            match super::screen_capture::create_capturer(config) {
                                Ok(mut back) => {
                                    super::screen_capture::negotiate_format(&mut *back, format);
                                    back.set_size_granularity(granularity);
                                    back.set_scale(applied_scale);
                                    back.set_size_ceiling(applied_ceiling);
                                    capturer = back;
                                }
                                Err(e) => {
                                    error!(
                                        "Session #{session_id}: capture is gone after a \
                                         failed display switch: {e:#}"
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }

                let (scale, fps) = *plan.lock().unwrap();
                if scale != applied_scale {
                    capturer.set_scale(scale);
                    applied_scale = scale;
                }

                // The guest's own window is a ceiling on what is worth sending.
                // Checked per tick rather than once, because it moves: resizing
                // the window or going fullscreen changes the answer.
                let ceiling = *viewport.lock().unwrap();
                if ceiling != applied_ceiling {
                    if capturer.set_size_ceiling(ceiling) {
                        match ceiling {
                            Some((w, h)) => info!(
                                "Session #{session_id}: guest can display {w}x{h} — \
                                 capping the stream to it"
                            ),
                            None => info!("Session #{session_id}: streaming at full size"),
                        }
                    }
                    applied_ceiling = ceiling;
                }

                // Pace to the target rate. If a frame took longer than the
                // interval — which it will at high resolutions — don't try to
                // catch up by running a burst; just start the next one now.
                let interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
                let now = std::time::Instant::now();
                if next_frame_at > now {
                    std::thread::sleep(next_frame_at - now);
                }
                next_frame_at = std::time::Instant::now() + interval;

                if produced_at.elapsed() >= TALLY_INTERVAL {
                    let secs = produced_at.elapsed().as_secs_f32().max(0.001);
                    // Separate from the encode thread's line because the two
                    // answer different questions: this one is whether the
                    // desktop is changing at all, which is the first thing to
                    // rule out and the last thing anyone thinks to check.
                    info!(
                        "Session #{session_id}: capture produced {:.1}/s, \
                         {:.1}/s unchanged, dropped {:.1}/s to a busy encoder",
                        produced as f32 / secs,
                        unchanged as f32 / secs,
                        dropped as f32 / secs,
                    );
                    produced = 0;
                    unchanged = 0;
                    dropped = 0;
                    produced_at = std::time::Instant::now();
                }

                // Read before the frame: the pointer moves on acquires where
                // the desktop does not change at all, and those return `None`
                // below and `continue` past everything after this.
                if let Some(update) = capturer.take_cursor()
                    && cursor_tx.send(update).is_err()
                {
                    break;
                }

                match capturer.capture_next() {
                    // Nothing changed. An idle desktop costs nothing to send,
                    // and the encode thread ticks on its own timer regardless.
                    Ok(None) => {
                        unchanged += 1;
                        continue;
                    }
                    Ok(Some(frame)) => {
                        produced += 1;
                        // Blocking would hold the newest frame behind an older
                        // one; dropping it means the next capture is fresher.
                        // Safe here and only here, because this is *before* the
                        // encoder — no inter-frame state depends on it.
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                dropped += 1;
                                trace!("Session #{session_id}: encoder busy, frame dropped");
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(e) => {
                        warn!("Session #{session_id}: capture failed: {e}");
                        break;
                    }
                }
            }
            debug!("Session #{}: capture thread stopped", session_id);
        })
        .expect("failed to spawn capture thread")
}

/// Let go of every touch contact, wherever it is being held.
///
/// Both sides, because a gesture can have been handed to an agent partway
/// through and each end holds only what it injected itself. Both are no-ops
/// when nothing is down, which is the ordinary case.
fn release_all_touches(injector: &dyn super::input_inject::InputInjector) {
    super::capture_follow::release_agent_touches();
    if let Err(e) = injector.release_touches() {
        debug!("Could not release touch contacts: {e:#}");
    }
}

/// Read and dispatch inbound messages until the stream closes.
///
/// Owns `recv` exclusively — see the module docs on cancel-safety.
#[allow(clippy::too_many_arguments)]
async fn run_reader<R>(
    mut recv: R,
    session_id: u32,
    sessions: Arc<SessionManager>,
    out_tx: mpsc::UnboundedSender<Message>,
    telemetry: SharedTelemetry,
    acks: SharedAck,
    viewport: SharedViewport,
    keyframe_wanted: KeyframeWanted,
    wanted_display: WantedDisplay,
    allowed_displays: AllowedDisplays,
    follows_guest: bool,
    injector: Arc<dyn super::input_inject::InputInjector>,
) where
    R: tokio::io::AsyncReadExt + Unpin + Send + 'static,
{
    // Which virtual display this session took for its own size, if it took one.
    // Per session rather than shared: see `follow_guest_size`.
    let claimed_display: ClaimedDisplay = Arc::new(std::sync::Mutex::new(None));
    // Whether the last touch to arrive was refused. A gesture in progress when
    // the guest's control is withdrawn has to be let go of here: the guest
    // cannot lift those contacts, because the message that would do it is the
    // one being refused.
    let mut touch_refused = false;

    loop {
        let data = match read_stream_message(&mut recv).await {
            Ok(Some(data)) => data,
            Ok(None) => break, // clean EOF
            Err(e) => {
                debug!("Session #{}: read ended: {}", session_id, e);
                break;
            }
        };

        match bincode::deserialize::<Message>(&data) {
            Ok(Message::Config(cfg)) => {
                info!(
                    "Session #{}: guest config fps={}, bitrate={}kbps, res={}x{}",
                    session_id, cfg.target_fps, cfg.max_bitrate_kbps, cfg.client_width, cfg.client_height
                );
                // A guest that has not worked out its own size yet sends zeroes;
                // treating that as a ceiling would stream a 0x0 picture.
                *viewport.lock().unwrap() = (cfg.client_width >= 2 && cfg.client_height >= 2)
                    .then_some((cfg.client_width, cfg.client_height));
                let window = (cfg.client_width, cfg.client_height);
                sessions.set_display_config(session_id, cfg);

                // The guest's window has just been reported, which is the only
                // moment its size is known. See `follow_guest_size`.
                if follows_guest && window.0 >= 2 && window.1 >= 2 {
                    follow_guest_size(
                        session_id,
                        &wanted_display,
                        window,
                        &claimed_display,
                    );
                }
            }
            Ok(Message::WatchDisplay(choice)) => {
                use super::protocol::DisplayChoice;
                use super::screen_capture::DisplayTarget;

                // # Answered whether or not it worked
                //
                // The guest's menu shows a selection, and a selection that does
                // not match what is being streamed is worse than no menu. So the
                // reply carries what is actually being shown, and says why when
                // that is not what was asked for.
                let allowed = allowed_displays.lock().unwrap().clone();
                let available: Vec<_> = super::screen_capture::displays()
                    .into_iter()
                    .filter(|d| allowed.allows(d.id))
                    .collect();
                let (showing, why) = match choice {
                    // # A window grant is a menu with no screens on it
                    //
                    // Switching *within* the granted set is allowed, and is
                    // what makes granting several windows worth doing. Anything
                    // else is not: a session confined to windows cannot be
                    // talked into a screen by asking for one, whatever the
                    // guest's menu happens to be showing.
                    DisplayChoice::Window(id) if allowed.allows_window(id) => {
                        info!("Session #{session_id}: guest asked for window {id}");
                        *wanted_display.lock().unwrap() = DisplayTarget::Window(id);
                        (DisplayChoice::Window(id), None)
                    }
                    // # "All of it" means all of the windows, here
                    //
                    // The same request a screen session uses to ask for the
                    // whole desk, answered with the whole of what *this* session
                    // was given. Nothing outside the grant becomes reachable by
                    // asking for everything — the composite is built from the
                    // granted list and no other window can enter it.
                    //
                    // Refused for a grant of one, where a grid of one window is
                    // the window with the overhead of composing it.
                    DisplayChoice::All if allowed.windows().len() > 1 => {
                        let ids = allowed.windows().to_vec();
                        info!(
                            "Session #{session_id}: guest asked for all {} shared windows",
                            ids.len()
                        );
                        *wanted_display.lock().unwrap() = DisplayTarget::Windows(ids);
                        (DisplayChoice::All, None)
                    }
                    _ if !allowed.windows().is_empty() => {
                        // Whatever it asked for, it is not on offer. The reply
                        // carries what is actually being shown so the guest's
                        // switcher corrects itself rather than showing a
                        // selection that never took.
                        let current = wanted_display.lock().unwrap().clone();
                        let showing = match current {
                            DisplayTarget::Window(id) => DisplayChoice::Window(id),
                            DisplayTarget::Windows(_) => DisplayChoice::All,
                            // Cannot happen — a window session's target is one
                            // window or several — but the reply has to say
                            // something, and the first granted window is the one
                            // it would have opened on.
                            _ => DisplayChoice::Window(
                                allowed.windows().first().copied().unwrap_or_default(),
                            ),
                        };
                        let why = (showing != choice)
                            .then(|| "this session is sharing windows, not screens".to_string());
                        if why.is_some() {
                            warn!(
                                "Session #{session_id}: refused {choice:?} — the grant is \
                                 {} window(s)",
                                allowed.windows().len()
                            );
                        }
                        (showing, why)
                    }
                    // A window nobody granted. Same reasoning as an unknown
                    // display id: a stale menu, not a thing to honour.
                    DisplayChoice::Window(id) => {
                        let why = format!("window {id} is not shared with this session");
                        warn!("Session #{session_id}: {why}");
                        let current = wanted_display.lock().unwrap().clone();
                        (showing_now(&current, &available), Some(why))
                    }
                    DisplayChoice::All if available.len() < 2 => {
                        // Nothing to compose. Not an error — one screen shown
                        // whole is what "all of them" means here — but the guest
                        // is told so its menu does not claim otherwise.
                        let why = "this host has one screen".to_string();
                        (DisplayChoice::One(available.first().map_or(0, |d| d.id)), Some(why))
                    }
                    DisplayChoice::All => {
                        info!("Session #{session_id}: guest asked for every screen");
                        *wanted_display.lock().unwrap() = DisplayTarget::All;
                        // The pointer maps against the whole desk now, which is
                        // the bounding box of every display rather than any one
                        // of them.
                        let left = available.iter().map(|d| d.x).min().unwrap_or(0);
                        let top = available.iter().map(|d| d.y).min().unwrap_or(0);
                        let right = available
                            .iter()
                            .map(|d| d.x + i32::from(d.width))
                            .max()
                            .unwrap_or(0);
                        let bottom = available
                            .iter()
                            .map(|d| d.y + i32::from(d.height))
                            .max()
                            .unwrap_or(0);
                        injector.set_shown(super::input_inject::Shown {
                            x: left,
                            y: top,
                            width: (right - left).clamp(0, u16::MAX as i32) as u16,
                            height: (bottom - top).clamp(0, u16::MAX as i32) as u16,
                        });
                        (DisplayChoice::All, None)
                    }
                    // Named `chosen`, not `display`: `use tracing::*` brings a
                    // `display` function into scope and the macros below resolve
                    // the identifier to it.
                    DisplayChoice::One(id) => match available.iter().find(|d| d.id == id) {
                        Some(chosen) => {
                            info!("Session #{session_id}: guest asked for {} ({id})", chosen.name);
                            *wanted_display.lock().unwrap() =
                                DisplayTarget::One(chosen.name.clone());
                            // The pointer has to move with the picture. Without
                            // this, everything the guest sends is still scaled
                            // against whichever display it was watching before.
                            injector.set_shown(super::input_inject::Shown {
                                x: chosen.x,
                                y: chosen.y,
                                width: chosen.width,
                                height: chosen.height,
                            });
                            (DisplayChoice::One(chosen.id), None)
                        }
                        None => {
                            // Far more likely a stale menu than a display the
                            // host has forgotten, so nothing changes.
                            let why =
                                format!("display {id} is not one of this host's {}", available.len());
                            warn!("Session #{session_id}: {why}");
                            let current = wanted_display.lock().unwrap().clone();
                            (showing_now(&current, &available), Some(why))
                        }
                    },
                };
                // # Only the answers that are already true are sent from here
                //
                // A refusal is final the moment it is decided, so it goes now.
                // An acceptance is not: `wanted_display` has been set and the
                // capture thread has yet to act on it, which takes as long as
                // building the new capture takes — the better part of a second
                // for a composite of several windows.
                //
                // Sent from here, it announced a view the host was still
                // producing the *old* pictures of. The guest cleared its "this
                // is on the way" indicator on the next frame, which was a frame
                // of what it had just left — reported as the loading screen
                // going away too early, showing a stale window.
                //
                // So the capture thread sends it when the switch has actually
                // happened. See `spawn_capture_thread`.
                if why.is_some() {
                    let _ = out_tx.send(Message::DisplayChanged(
                        super::protocol::DisplayChanged { showing, why },
                    ));
                }
            }
            Ok(Message::Input(event)) => {
                // Re-read grants per event: they can change at any point during
                // the session, and mouse/keyboard are permitted independently.
                let device = event.device();
                let touch = matches!(event, super::protocol::InputEvent::Touch(_));
                if !sessions.grants(session_id).is_some_and(|g| g.allows(device)) {
                    trace!("Session #{}: {:?} input dropped — not granted", session_id, device);
                    if touch && !touch_refused {
                        touch_refused = true;
                        release_all_touches(injector.as_ref());
                    }
                    continue;
                }
                if touch {
                    touch_refused = false;
                }
                if let Err(e) = inject_input(injector.as_ref(), &event) {
                    warn!("Session #{}: input injection failed: {}", session_id, e);
                }
            }
            Ok(Message::FrameAck(sequence)) => acks.lock().unwrap().record(sequence),
            Ok(Message::RequestKeyframe) => {
                info!("Session #{session_id}: guest cannot decode — sending a keyframe");
                keyframe_wanted.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(Message::Telemetry(report)) => {
                // Sanitised before it reaches the controller: these numbers come
                // from the other machine and steer this one's encoder, so a
                // peer sending NaN or a negative decode time must not be able to
                // drive the stream into its floor.
                let report = sanitize_telemetry(report);
                *telemetry.lock().unwrap() = Some(report);
                // Also kept per session, so the host's own UI can say how the
                // link is doing without the guest being the only one who knows.
                sessions.set_link(session_id, report);
            }
            Ok(Message::Ping(ts)) => {
                let host_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if out_tx.send(Message::Pong { echo: ts, host_ms }).is_err() {
                    break;
                }
            }
            Ok(Message::SessionEnd(se)) => {
                info!("Session #{}: guest disconnected ({})", session_id, se.reason);
                break;
            }
            Ok(_) => trace!("Session #{}: ignored host-bound message type", session_id),
            Err(e) => {
                error!("Session #{}: deserialize error: {:?}", session_id, e);
                break;
            }
        }
    }
}

/// Clamp a peer's self-reported telemetry into believable ranges.
///
/// This crosses a trust boundary: the guest's numbers decide how the host
/// encodes, so an unclamped report is a lever a remote peer can pull on this
/// machine. NaN would poison every comparison in the controller, and an absurd
/// decode time would pin the stream at its floor — neither is a large attack,
/// but neither has any legitimate cause either.
fn sanitize_telemetry(
    mut report: super::protocol::GuestTelemetry,
) -> super::protocol::GuestTelemetry {
    let clamp = |v: f32, max: f32| if v.is_finite() { v.clamp(0.0, max) } else { 0.0 };
    report.decode_ms = clamp(report.decode_ms, 10_000.0);
    report.render_ms = clamp(report.render_ms, 10_000.0);
    report.displayed_fps = clamp(report.displayed_fps, 1_000.0);
    report.rtt_ms = clamp(report.rtt_ms, 60_000.0);
    report
}

/// Translate a protocol input event into an OS injection call.
///
/// # The secure desktop takes its own input
///
/// `SendInput` reaches the desktop of the thread that calls it, so while a
/// consent prompt is up this process cannot deliver anything: the guest would
/// watch a prompt it could not answer, which is barely better than not seeing
/// it. When an agent is supplying frames it takes the clicks too, and the two
/// are deliberately the same decision — input following frames is the only
/// arrangement where a click cannot land on a desktop the guest is not looking
/// at.
///
/// The check costs a mutex and answers `false` everywhere else, including in
/// the agent itself, which reuses this function and never has a link of its
/// own.
pub fn inject_input(
    injector: &dyn super::input_inject::InputInjector,
    event: &super::protocol::InputEvent,
) -> Result<()> {
    use super::input_inject::{
        KeyInput, MouseButtonInput, MousePosition, ScrollInput, TouchInput,
    };
    use super::protocol::InputEvent;

    let via_agent = super::capture_follow::send_input_to_agent(event);

    // # A gesture that changes hands mid-way
    //
    // Touch is the one input that leaves something behind. An elevated window
    // arriving while a finger is down moves the route to the agent, and the
    // contacts this process is holding then have nothing left to lift them —
    // they stay pressed against the screen until something resets the
    // injection. The same in reverse when that window goes away.
    //
    // So on a change of route, the side being left is told to let go. The
    // events that follow arrive as movement for a contact the new side does
    // not know, which it reads as a press: the gesture continues rather than
    // being cut in half.
    if matches!(event, InputEvent::Touch(_))
        && super::capture_follow::touch_route_changed(via_agent)
    {
        if via_agent {
            let _ = injector.release_touches();
        } else {
            super::capture_follow::release_agent_touches();
        }
    }

    if via_agent {
        return Ok(());
    }

    match event {
        InputEvent::MouseMove(m) => injector.move_mouse(MousePosition { x: m.x, y: m.y }),
        InputEvent::MouseMotion(m) => injector.move_mouse_by(m.dx.into(), m.dy.into()),
        InputEvent::MouseButton(b) => injector.mouse_button(MouseButtonInput {
            button: b.button,
            pressed: b.pressed,
        }),
        InputEvent::Scroll(s) => injector.scroll(ScrollInput { dx: s.dx, dy: s.dy }),
        InputEvent::Key(k) => injector.key(KeyInput {
            scancode: k.scancode,
            extended: k.extended,
            keycode: k.keycode,
            pressed: k.pressed,
        }),
        InputEvent::Touch(t) => injector.touch(TouchInput {
            id: t.id,
            phase: t.phase,
            x: t.x,
            y: t.y,
            pressure: t.pressure,
        }),
    }
}

// ── Guest side ───────────────────────────────────────────────

/// A guest client that connects to a host via iroh P2P using a passcode ticket.
pub struct GuestClient {
    connection: Option<iroh::endpoint::Connection>,
    /// Which address lookup located this host. See [`super::discovery::Found`].
    found: super::discovery::Found,
}

/// Where a connection attempt has got to.
///
/// # Why "Connecting" was not enough
///
/// Dialling a host is four or five separate things that can each fail on their
/// own, and on a link where the relays are interfered with they fail at quite
/// different rates. A user watching one word for twenty seconds cannot tell
/// "this host is switched off" from "the relay is unreachable from here" from
/// "the code has expired", and those want three different responses.
///
/// The steps are reported as they are reached, so the last one shown is the one
/// that did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialStep {
    /// Binding this machine's own endpoint.
    Starting,
    /// Waiting to reach a relay, because the code carried no address to try.
    ReachingRelay,
    /// Dialling the relay named in the code.
    ViaRelay,
    /// Dialling an address this host answered on before, with no relay involved.
    Direct,
    /// Reached the host, and waiting for hole punching to open a direct path.
    ///
    /// Its own step because it is the one that takes real time and the one most
    /// likely to be where an attempt ends. Everything before it is a handshake
    /// measured in hundreds of milliseconds; this is two machines trying to
    /// find a route to each other through whatever is between them.
    Punching,
    /// Connected at the transport level; sending the passcode.
    Authenticating,
}

impl DialStep {
    /// What to show while this step is the one in progress.
    pub fn label(self) -> &'static str {
        super::i18n::text(match self {
            Self::Starting => "step.starting",
            Self::ReachingRelay => "step.reaching_relay",
            Self::ViaRelay => "step.via_relay",
            Self::Direct => "step.direct",
            Self::Punching => "step.punching",
            Self::Authenticating => "step.authenticating",
        })
    }

    /// What to say when the attempt gave up here.
    ///
    /// Phrased as the thing that did not happen rather than as an error code,
    /// because the useful next action differs at every step.
    pub fn stalled_at(self) -> &'static str {
        super::i18n::text(match self {
            Self::Starting => "stalled.starting",
            Self::ReachingRelay => "stalled.reaching_relay",
            Self::ViaRelay => "stalled.via_relay",
            Self::Direct => "stalled.direct",
            Self::Punching => "stalled.punching",
            Self::Authenticating => "stalled.authenticating",
        })
    }
}

impl GuestClient {
    /// Connect to the host identified by its PublicKey (EndpointId).
    pub async fn connect(host_public_key: &iroh::PublicKey) -> Result<Self> {
        Self::connect_via(host_public_key, None).await
    }

    /// Connect, using a relay the code supplied rather than looking one up.
    ///
    /// # Why the relay matters more than it looks
    ///
    /// An endpoint id is a public key, not an address. With only that, iroh
    /// asks its lookup service — a DNS query — to translate it. Where that
    /// query does not return, the connection does not fail: it retries with a
    /// growing backoff and waits indefinitely, which from outside is an
    /// application that simply never connects.
    ///
    /// A relay in the code removes that step entirely: the guest dials the
    /// relay and hole punching proceeds from there.
    pub async fn connect_via(
        host_public_key: &iroh::PublicKey,
        relay_url: Option<&str>,
    ) -> Result<Self> {
        Self::connect_reporting(host_public_key, relay_url, &|_| {}).await
    }

    /// As [`Self::connect_via`], saying which step it has reached as it goes.
    ///
    /// `on_step` is called from the dialling task, so it must be cheap and must
    /// not block — publishing to shared state is what it is for. The last step
    /// reported before a failure is the one that did not finish, which is the
    /// whole point: see [`DialStep`].
    pub async fn connect_reporting(
        host_public_key: &iroh::PublicKey,
        relay_url: Option<&str>,
        on_step: &(dyn Fn(DialStep) + Send + Sync),
    ) -> Result<Self> {
        /// How long to wait for the endpoint to report itself online.
        ///
        /// Not a requirement, a courtesy. Coming online means reaching a relay
        /// and finishing a network report, and on a filtered connection that
        /// can take a long time or never happen. With a relay in the code there
        /// is nothing left to discover, so waiting past this point buys
        /// nothing — and waiting forever is what "it just never connects" was.
        const ONLINE_WAIT: Duration = Duration::from_secs(5);

        /// How long one dial may take before it is abandoned and asked again.
        ///
        /// A cold path may need a relay handshake and hole punching first, and
        /// iroh retries inside this with a growing backoff. Shorter than it was,
        /// because it is no longer the end of the attempt: a dial that spends
        /// this without an answer is now one ask among several, and a budget
        /// spent on more asks finds more hosts than a budget spent waiting
        /// longer on one.
        const DIAL_TIMEOUT: Duration = Duration::from_secs(12);

        /// How long to keep trying before calling the host unreachable.
        ///
        /// Discovery is allowed to be slow, and so is a link that needs several
        /// asks. Waiting costs nothing that matters: how fast the session then
        /// runs is a property of the path it found, not of how long finding it
        /// took, and the progress graph says what is being waited on.
        const FIND_WAIT: Duration = Duration::from_secs(45);

        /// How long to leave between asking again.
        const FIND_RETRY: Duration = Duration::from_secs(2);

        // The budget has to leave room for at least one whole attempt *after*
        // discovery's run, or the saved address would never be reached and the
        // fallback would silently stop existing. Checked here rather than in a
        // test because it is three constants, and a mismatch should fail the
        // build rather than wait for somebody to connect on a filtered link.
        const _: () = assert!(
            DISCOVERY_GRACE.as_secs() + DIAL_TIMEOUT.as_secs() < FIND_WAIT.as_secs(),
            "no room left to try the address this host answered on last time"
        );

        // The cached resolver, as everywhere else — this is the path a user on
        // a censored link actually presses, and the one where three seconds of
        // DNS per relay is the difference between connecting and not.
        on_step(DialStep::Starting);
        // The guest needs these more than the host does: this is the side that
        // has to *find* somebody, and mDNS answers that on a local network
        // without the relay this dial would otherwise have to reach first.
        //
        // Shared rather than bound per dial — see `OUTGOING_ENDPOINT`, which
        // explains why two of them under one identity is two machines with the
        // same name.
        let (endpoint, lookups) = outgoing_endpoint().await?;
        let endpoint = endpoint.clone();

        // Only worth naming as a step when there is nothing else to go on. With
        // a relay in the code or an address on file, this wait is a courtesy
        // and the user should not be told the connection depends on it.
        // No longer excused by having an address on file. That address is not
        // tried until everything else has come back empty, so a dial with one
        // still depends on this wait and the graph should say so.
        if relay_url.is_none_or(str::is_empty) {
            on_step(DialStep::ReachingRelay);
        }
        let online = tokio::time::timeout(ONLINE_WAIT, endpoint.online()).await.is_ok();
        if !online {
            warn!(
                "This endpoint has not come online after {ONLINE_WAIT:?} — dialling anyway. \
                 Expected where relays are filtered; a code that carries one needs no lookup."
            );
        }

        let mut addr = iroh::EndpointAddr::new(*host_public_key);
        match relay_url.filter(|url| !url.is_empty()) {
            Some(url) => match url.parse() {
                Ok(parsed) => {
                    addr = addr.with_relay_url(parsed);
                    info!("Connecting to host via {url}");
                }
                // A code from a newer or hand-edited build. The lookup path
                // still exists, so this is a warning rather than a refusal.
                Err(e) => warn!("Ignoring the relay address in this code ({e}) — looking it up"),
            },
            None => info!("Connecting to host (no relay in the code — looking it up)"),
        }

        // # There is no address from last time any more
        //
        // Tain used to keep the address a host answered on and hand it to the
        // next dial. Three attempts were made to make that safe — added
        // alongside the relay, then only when no lookup answered, then only
        // after a discovery grace — and each one was reported as still breaking
        // connections that worked the moment the saved address was deleted.
        //
        // The idea itself was wrong. An address is not evidence about the
        // present: a host that has changed network, been given a different port
        // by its NAT, or gone behind a tunnel leaves one behind that still
        // *routes*, to a NAT that now maps it to somebody else or to nothing at
        // all. Those do not lose a race quickly — they take the dial with them.
        // And the cache could only ever be refreshed by a *successful* session,
        // so a stale entry was cleared by the one thing it was preventing.
        //
        // It was also duplicating work that is already done better. iroh keeps
        // its own path state per endpoint; mDNS answers on a local network with
        // no relay and no internet; and the DHT record carries dialable
        // addresses rather than only a relay — see `discovery::publishable`,
        // which exists for exactly the case a remembered address was supposed
        // to cover. Where the host *is* is a question with three live answers,
        // and a fourth stale one was not helping any of them.
        //
        // So: the relay in the code, mDNS, DNS, the DHT. Nothing else.
        on_step(DialStep::ViaRelay);

        // # "Nobody knows yet" is not "nobody knows"
        //
        // `connect` already waits on lookups that are still in flight, so a
        // host that becomes findable part way through a dial is found without
        // any help from here — mDNS holds its stream open, and a host that
        // appeared ten seconds into a dial was picked up on the first attempt.
        //
        // What it does not wait for is lookups that *complete* with nothing.
        // Then it returns `NoAddress` at once, with the whole dial budget
        // unspent. That is not a hypothetical: an endpoint findable only
        // through the mainline DHT does exactly this, because a freshly
        // published record has to settle on the nodes that later get asked for
        // it — measured at about eight seconds and two asks. A guest whose
        // mDNS could not start and whose DNS is blocked has only that lookup,
        // and one attempt would call the host offline.
        //
        // Discovery taking a while does not matter. What the session runs at
        // afterwards is decided by the path it found, not by how long finding
        // it took — so where more time is the whole fix, this spends it.
        //
        // # One failure is not worth a second of the budget
        //
        // Everything below retries, on the reasoning that a route which did not
        // work this second may work the next. That reasoning does not cover
        // dialling *ourselves*: iroh refuses it outright, instantly, and will go
        // on refusing it. Left to the loop it burned the whole 45-second budget
        // over 24 identical refusals, and then the outer retry did it twice more
        // — two and a half minutes to be told something knowable before the
        // first attempt.
        //
        // It should not be reachable any more: this machine hosts on one key and
        // dials with another derived from it, so its own code is a legitimate
        // thing to paste. The check stays because the alternative to a clear
        // answer here is that same silent quarter-hour.
        if addr.id == endpoint.id() {
            return Err(anyhow::anyhow!(
                "this code is for the machine it was pasted on, and its host and client are \
                 the same endpoint — which iroh cannot dial"
            ));
        }

        let started = std::time::Instant::now();
        let deadline = started + FIND_WAIT;
        let mut attempts = 0u32;
        // The most recent failure, which is what gets reported if the budget
        // runs out. The last one rather than the first: by then the earlier
        // ones are "we had not found it yet", and the useful sentence is what
        // was still wrong at the end.
        let mut last;
        let conn = loop {
            attempts += 1;

            let why: anyhow::Error =
                match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr.clone(), ALPN))
                    .await
                {
                    Ok(Ok(conn)) => break conn,
                    Ok(Err(e)) => anyhow::Error::new(e),
                    // # A dial that hangs is an ask that did not answer
                    //
                    // It used to end the whole attempt. On a link where the
                    // relay is filtered that is the *ordinary* outcome of the
                    // first ask, so the one network this software is written
                    // for got a single try and a failure — with the rest of the
                    // budget unspent and the DHT never consulted.
                    Err(_elapsed) => {
                        anyhow::anyhow!("no answer within {DIAL_TIMEOUT:?}")
                    }
                };

            if attempts == 1 {
                debug!("Nowhere to reach this host yet ({why:#}) — still looking");
            }
            last = why;

            // # Every failure is retried, while there is time
            //
            // This used to retry only `NoAddress`, on the reasoning that
            // anything else was a definite refusal and retrying would turn a
            // clear failure into a slow one. On a good link that is true. On a
            // filtered one it is not: a relay handshake that fails is not a
            // refusal, it is a route that did not work this second, and there
            // are other routes that might. The deadline is what stops this
            // being unbounded — not the first discouraging answer.
            if std::time::Instant::now() >= deadline {
                // # What was actually tried, said once, at the end
                //
                // Several rounds of "it cannot find my other PC" have been
                // diagnosed by reading code rather than a log, because nothing
                // anywhere said which ways of finding a host this machine even
                // had. Two PCs on one LAN that cannot see each other is mDNS
                // not running; the same pair on a filtered link is the DHT not
                // running. The log said neither.
                warn!(
                    "Could not place this host in {:?} over {attempts} attempt(s). \
                     Ways of finding it on this machine: mDNS {}, DNS {}, DHT {}. \
                     Relay in the code: {}.",
                    started.elapsed(),
                    if lookups.mdns { "on" } else { "OFF" },
                    if lookups.dns { "on" } else { "OFF" },
                    if lookups.dht { "on" } else { "OFF" },
                    if relay_url.is_some_and(|url| !url.is_empty()) { "yes" } else { "no" },
                );
                return Err(anyhow::Error::context(
                    last,
                    format!(
                        "gave up after {attempts} attempt(s) over {:?}",
                        started.elapsed()
                    ),
                ));
            }
            if !online {
                let _ = tokio::time::timeout(ONLINE_WAIT, endpoint.online()).await;
            }
            tokio::time::sleep(FIND_RETRY).await;
        };
        if attempts > 1 {
            debug!("Found the host on attempt {attempts}");
        }
        info!("Connected to host: {}", conn.remote_id());

        // # The relay has now done its job
        //
        // Reaching the host is not the same as being able to stream from it.
        // Every iroh connection starts on the relay, and the relay is there to
        // carry the handshake and the address exchange that let the two
        // machines punch a direct path. Once that has happened the relay has
        // nothing left to contribute, and if it has *not* happened the
        // connection is a shared, rate-limited public server pretending to be a
        // video link.
        //
        // So the wait happens here, before the caller is handed anything it
        // could stream over.
        on_step(DialStep::Punching);
        match await_direct_path(&conn, DIRECT_PATH_WAIT).await {
            DirectPath::Established(path) => {
                info!(
                    "Direct path to host: {} ({:.0} ms round trip)",
                    path.remote(),
                    path.rtt_ms()
                );
            }
            DirectPath::StillRelayed { via } => {
                conn.close(0u32.into(), b"relay-only");
                anyhow::bail!("{}", relay_only_message(&via));
            }
            DirectPath::Lost => {
                anyhow::bail!(
                    "The host stopped answering while a direct path was being opened."
                );
            }
        }

        // Cloned because the lookups outlive this connection now: they belong
        // to the shared endpoint, and a second session will read the same
        // record to say how *it* found its host.
        Ok(Self { connection: Some(conn), found: lookups.found.clone() })
    }

    /// Which address lookup found this host.
    ///
    /// `None` when nothing had to look: a code carrying a relay, or an address
    /// remembered from last time, is an answer the guest already had.
    pub fn found_via(&self) -> Option<super::discovery::Branch> {
        self.found.branch(self.connection.as_ref()?.remote_id())
    }

    /// Connect using a raw 32-byte endpoint ID.
    pub async fn connect_from_bytes(endpoint_id_bytes: &[u8; 32]) -> Result<Self> {
        let public_key = iroh::PublicKey::from_bytes(endpoint_id_bytes)
            .context("Invalid endpoint ID bytes")?;
        Self::connect(&public_key).await
    }

    /// Connect to whichever host a connection code addresses.
    ///
    /// This is the whole point of the code format: the guest needs no separate
    /// ticket, because the address travels with the secret.
    pub async fn connect_with_code(code: &ConnectionCode) -> Result<Self> {
        Self::connect_with_code_reporting(code, &|_| {}).await
    }

    /// As [`Self::connect_with_code`], saying which step it has reached.
    pub async fn connect_with_code_reporting(
        code: &ConnectionCode,
        on_step: &(dyn Fn(DialStep) + Send + Sync),
    ) -> Result<Self> {
        let public_key =
            iroh::PublicKey::from_bytes(&code.endpoint_id).context("Invalid endpoint ID bytes")?;
        // The relay the host put in the code, so no lookup is needed to find
        // it. Codes issued before this existed carry none, and fall back to the
        // lookup path.
        Self::connect_reporting(&public_key, code.relay_url.as_deref(), on_step).await
    }

    /// Take ownership of the underlying connection's stream pair, authenticating first.
    ///
    /// The guest is the side that **opens** the stream. `client_name` is what
    /// this machine calls itself; `None` stays anonymous and the host shows a
    /// generated label instead.
    pub async fn authenticate(
        &mut self,
        passcode: &str,
        client_name: Option<&str>,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        let conn = self.connection.as_ref().context("Not connected")?;
        let (mut send, recv) = conn.open_bi().await.context("Failed to open bi stream")?;

        write_stream_message(
            &mut send,
            &Message::Auth {
                passcode: passcode.to_string(),
                // Advisory only — the host assigns the real id and returns it in SessionStart.
                session_id: 0,
                client_name: client_name
                    .and_then(super::protocol::sanitize_device_name),
            },
        )
        .await?;

        info!("Sent authentication ({})", redact(passcode));
        Ok((send, recv))
    }

    /// The live connection, for opening or accepting further streams.
    ///
    /// Cloning is cheap — it is a handle, not the connection state.
    pub fn connection(&self) -> Option<iroh::endpoint::Connection> {
        self.connection.clone()
    }

    /// How this connection is actually carried: a direct path, or a relay.
    ///
    /// Worth surfacing, because it is the single biggest determinant of whether
    /// a session is usable and it is otherwise invisible. A relayed path runs
    /// through a shared public server — fine for a few hundred kbit/s of control
    /// traffic, hopeless for 17 Mbit/s of video — and a user watching a
    /// stuttering picture has no way to tell that from a slow machine.
    pub fn connection_path(&self) -> Option<ConnectionPath> {
        path_of(self.connection.as_ref()?)
    }

    /// Close this session's connection.
    ///
    /// The endpoint underneath is shared by every session this process has open
    /// and is deliberately left alone — see [`OUTGOING_ENDPOINT`]. Closing it
    /// here used to be right, when each connection had one of its own; now it
    /// would take down the other tabs.
    pub async fn shutdown(&self) -> Result<()> {
        if let Some(ref connection) = self.connection {
            connection.close(0u32.into(), b"done");
        }
        Ok(())
    }
}

/// How a connection reaches the other machine.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionPath {
    /// Peer to peer. What hole punching is for, and what video needs.
    Direct { rtt_ms: f32, remote: String },
    /// Bounced through a public relay because hole punching did not succeed.
    Relay { host: String, rtt_ms: f32 },
}

impl ConnectionPath {
    /// Short label for the stats line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Direct { .. } => "direct",
            Self::Relay { .. } => "relay",
        }
    }

    /// Whether this path can be expected to carry video.
    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    /// Transport-level round trip, which is measured rather than inferred.
    pub fn rtt_ms(&self) -> f32 {
        match self {
            Self::Direct { rtt_ms, .. } | Self::Relay { rtt_ms, .. } => *rtt_ms,
        }
    }

    /// The address the other machine is actually reached at.
    ///
    /// Worth showing rather than hiding: a round trip that looks too long for
    /// the distance is usually explained by this line. A private or
    /// carrier-grade address on a link between two houses means the packets are
    /// going through a tunnel — a VPN on either machine — and the latency being
    /// paid is that tunnel's, not the internet's.
    pub fn remote(&self) -> &str {
        match self {
            Self::Direct { remote, .. } | Self::Relay { host: remote, .. } => remote,
        }
    }

    /// Whether the peer address looks like a VPN or other tunnel rather than a
    /// public one.
    ///
    /// Only ever used to *ask a question* in the UI, never to change behaviour:
    /// these ranges are also what a LAN session legitimately uses, and being
    /// wrong about a machine in the next room must not cost anything.
    pub fn looks_tunnelled(&self) -> bool {
        let host = self.remote();
        let Some(addr) = host.rsplit_once(':').map(|(a, _)| a.trim_matches(['[', ']'])) else {
            return false;
        };
        let Ok(ip) = addr.parse::<std::net::IpAddr>() else { return false };
        match ip {
            std::net::IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                // RFC1918 and the carrier-grade range Tailscale and friends use.
                v4.is_private() || (a == 100 && (64..128).contains(&b))
            }
            // Unique local addresses: the IPv6 equivalent.
            std::net::IpAddr::V6(v6) => (v6.octets()[0] & 0xfe) == 0xfc,
        }
    }

    /// What to tell the user when the path is the problem.
    pub fn advice(&self) -> String {
        match self {
            Self::Direct { .. } => super::i18n::text("path.direct").to_string(),
            Self::Relay { host, .. } => super::i18n::format("path.relayed", &[("host", host)]),
        }
    }
}

/// Which path a connection settled on, once it has had time to settle.
#[derive(Debug, Clone, PartialEq)]
pub enum DirectPath {
    /// Hole punching worked. This is the path the video will take.
    Established(ConnectionPath),
    /// It never left the relay in the time allowed.
    StillRelayed { via: String },
    /// The connection ended before it settled either way.
    Lost,
}

/// How long hole punching gets before a connection is refused as relay-only.
///
/// # Why a session is refused rather than relayed
///
/// A relay is a shared public server carrying every other connection that
/// landed on it, and it is rate limited. That is ample for the few hundred
/// kbit/s of control traffic a relay exists to carry — the handshake, and the
/// address exchange that lets two machines find each other — and hopeless for
/// the ten to twenty Mbit/s of a desktop stream.
///
/// A session that runs over one is not a degraded session, it is an unusable
/// one: the measured example collapsed to a third of full resolution at 8 fps
/// and stayed there. Failing to connect at least says so, and says it in a
/// second rather than after a minute of watching a slideshow.
///
/// Generous, because the whole point is to wait rather than to give up: longer
/// than the eight seconds the status chip allowed, long enough for a punch
/// across an ocean and for a couple of iroh's own retries, and still short
/// enough to be a connection attempt rather than a hang.
pub const DIRECT_PATH_WAIT: Duration = Duration::from_secs(25);

/// How often the selected path is re-read while waiting for one.
const PATH_POLL: Duration = Duration::from_millis(250);

/// What the host adds to [`DIRECT_PATH_WAIT`] before giving up on a guest.
///
/// Both ends wait for the same punch, so both deadlines expire at nearly the
/// same moment — and whoever closes first decides what the *user* is told. The
/// guest is the one with something to say ("no direct path opened, here is what
/// to check"); the host closing first would replace that with the connection
/// vanishing mid-wait, which explains nothing.
///
/// The guest's timer already starts earlier — the host does not know a
/// connection exists until the handshake it dialled has completed — but only by
/// a handshake, and a handshake is not a margin. This is.
const HOST_PUNCH_MARGIN: Duration = Duration::from_secs(5);

/// How this connection is carried right now, if it is carrying anything.
///
/// A free function because both ends need the same answer: the guest to decide
/// whether to go on, the host to decide whether to serve.
pub fn path_of(conn: &iroh::endpoint::Connection) -> Option<ConnectionPath> {
    let paths = conn.paths();
    // The *selected* path is the one carrying data. A connection commonly holds
    // an idle relay path alongside a direct one, so reporting the first path
    // found would say "relay" for a healthy direct session.
    let selected = paths.iter().find(|p| p.is_selected()).or_else(|| paths.iter().next())?;
    let rtt_ms = selected.rtt().as_secs_f32() * 1000.0;

    Some(if selected.is_relay() {
        ConnectionPath::Relay { host: format!("{}", selected.remote_addr()), rtt_ms }
    } else {
        // The address is carried for direct paths too, because "direct" only
        // means "not through an iroh relay" — it says nothing about the route
        // those packets then take. Two machines both on a VPN get a direct path
        // *to their VPN addresses*, and the round trip is the tunnel's, which
        // can be several times the physical one. That reads as "direct, 110 ms"
        // with nothing on screen to explain it.
        ConnectionPath::Direct { rtt_ms, remote: format!("{}", selected.remote_addr()) }
    })
}

/// Wait for hole punching to produce a direct path.
///
/// Every iroh connection starts on the relay and upgrades if it can, so the
/// path at connect time is not an answer — it is a stopwatch that has not
/// started. This runs the stopwatch.
///
/// The relay is left doing exactly what it is for while this waits: carrying
/// the handshake and the address exchange that make the punch possible. What
/// it is not allowed to do is carry the session afterwards.
pub async fn await_direct_path(conn: &iroh::endpoint::Connection, limit: Duration) -> DirectPath {
    let deadline = std::time::Instant::now() + limit;
    let mut last_relay = None;

    loop {
        // Only a path iroh has actually *selected* counts here. `path_of` falls
        // back to reporting whatever path exists when none is selected yet,
        // which is the right answer for a status line and the wrong one for a
        // gate: a direct candidate that has not been chosen is carrying nothing.
        let carrying_direct = conn.paths().iter().any(|p| p.is_selected() && !p.is_relay());
        match path_of(conn) {
            Some(path) if carrying_direct => return DirectPath::Established(path),
            Some(ConnectionPath::Relay { host, .. }) => last_relay = Some(host),
            // No path at all means the connection has nothing open on it. That
            // is normal for the first poll or two and terminal after that, so
            // it is the close reason rather than the path that decides.
            _ => {}
        }
        if conn.close_reason().is_some() {
            return DirectPath::Lost;
        }
        if std::time::Instant::now() >= deadline {
            return match last_relay {
                Some(via) => DirectPath::StillRelayed { via },
                None => DirectPath::Lost,
            };
        }
        tokio::time::sleep(PATH_POLL).await;
    }
}

/// What to tell someone whose connection would not leave the relay.
///
/// One sentence on what happened and three on what to do about it, because the
/// remedies are not guessable: two of them are somebody else's machine, and the
/// one that most often works — connecting once on a network where it succeeds —
/// looks like it has nothing to do with the problem.
fn relay_only_message(via: &str) -> String {
    super::i18n::format(
        "path.relay_only",
        &[("via", via), ("wait", &format!("{DIRECT_PATH_WAIT:?}"))],
    )
}

#[cfg(test)]
mod path_tests {
    use super::ConnectionPath;

    /// A refusal has to leave the user with something to try.
    ///
    /// This is the one message in the app that reports a working connection
    /// being thrown away on purpose, which is a hard thing to read as anything
    /// but a bug. It has to name the relay it is refusing, say why a relay is
    /// not good enough, and give the remedies — none of which are guessable,
    /// since two of them are somebody else's machine and the one that most
    /// often works (connect once where it succeeds, so the address is
    /// remembered) looks unrelated to the problem.
    #[test]
    fn refusing_a_relay_says_what_to_do_about_it() {
        let message = super::relay_only_message("relay:https://euc1-1.relay.n0.iroh.link./");

        assert!(
            message.contains("euc1-1.relay.n0.iroh.link"),
            "the refusal does not say which relay it refused: {message}"
        );
        for remedy in ["VPN", "NAT", "firewall", "same network"] {
            assert!(
                message.contains(remedy),
                "the refusal never mentions {remedy}, which is one of the things to check: \
                 {message}"
            );
        }
        assert!(
            message.contains("tried first next time"),
            "the refusal does not tell the user that connecting once from a working network \
             fixes it for later, which is the remedy that actually resolves this: {message}"
        );
    }

    fn direct(remote: &str) -> ConnectionPath {
        ConnectionPath::Direct { rtt_ms: 90.0, remote: remote.to_string() }
    }

    /// A "direct" path can still be a tunnel, and the address is what says so.
    ///
    /// # Why this exists
    ///
    /// A session between two nearby cities reported "direct" with a 70–130 ms
    /// round trip, and there was nothing on screen to explain the gap. The
    /// label was accurate — no relay was involved — but it was read as "the
    /// short route", which it does not mean. These ranges are the ones a VPN
    /// hands out.
    #[test]
    fn private_and_carrier_grade_peers_are_flagged_as_tunnelled() {
        assert!(direct("10.2.0.7:41641").looks_tunnelled());
        assert!(direct("192.168.1.44:9000").looks_tunnelled());
        assert!(direct("172.16.9.1:9000").looks_tunnelled());
        // Tailscale and other CGNAT-range overlays.
        assert!(direct("100.101.102.103:41641").looks_tunnelled());
        assert!(direct("[fd7a:115c:a1e0::1]:41641").looks_tunnelled());
    }

    /// A public peer is the ordinary case and must never be second-guessed.
    #[test]
    fn public_peers_are_not_flagged() {
        assert!(!direct("203.0.113.9:41641").looks_tunnelled());
        assert!(!direct("[2001:db8::1]:41641").looks_tunnelled());
        // 100.x outside the carrier-grade block is ordinary public space.
        assert!(!direct("100.200.1.1:41641").looks_tunnelled());
        // Nothing parseable: say nothing rather than guess.
        assert!(!direct("somewhere").looks_tunnelled());
    }

    /// The flag is advisory only — it must not change what `is_direct` says,
    /// which is what the stream logic keys off.
    #[test]
    fn a_tunnelled_path_is_still_a_direct_one() {
        let path = direct("10.0.0.2:41641");
        assert!(path.is_direct());
        assert_eq!(path.label(), "direct");
    }
}

/// Fixed pause before telling a caller their code is wrong.
const INVALID_PROBE_DELAY: Duration = Duration::from_millis(400);

/// Checks saved codes for validity and reachability, without connecting.
///
/// Owns **one** endpoint for all probes. Binding an endpoint costs a socket, a
/// keypair and a relay handshake, so doing it per probe would make checking six
/// saved rows more expensive than an actual session.
pub struct Prober {
    endpoint: iroh::Endpoint,
}

/// What a probe found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The host answered and the code still works.
    Valid {
        host_name: Option<String>,
        needs_approval: bool,
    },
    /// The host answered, but this code is not one it accepts any more —
    /// almost always because it regenerated after the last disconnect.
    Stale,
    /// No answer, and what stopped it.
    ///
    /// `why` is the sentence a card can show: "offline" alone cannot separate a
    /// host that is switched off from a relay that is blocked from here, and
    /// those want opposite responses from whoever is reading it. Phrased as the
    /// thing that did not happen — see [`DialStep::stalled_at`], which is where
    /// most of these come from, so that a card and a session window explain the
    /// same failure the same way.
    Unreachable { why: String },
}

impl Prober {
    pub async fn bind() -> Result<Self> {
        // A host on this network should show as online without the answer
        // having to come back from another continent to say so.
        let (endpoint, _lookups) =
            super::discovery::bind(super::discovery::endpoint_builder()).await?;
        endpoint.online().await;
        Ok(Self { endpoint })
    }

    /// Ask a host whether it is there and whether this code still works.
    ///
    /// Never raises an approval request on the host, and never fails the caller:
    /// every outcome, including "could not reach it", is an answer worth showing
    /// on the card.
    pub async fn probe(&self, code: &ConnectionCode, timeout: Duration) -> ProbeOutcome {
        match tokio::time::timeout(timeout, self.try_probe(code)).await {
            Ok(Ok(outcome)) => outcome,
            // The reason travels with the answer now. A card that says a host
            // is offline is making a claim, and the claim is worth a lot more
            // when it says which part of reaching it did not work — the relay
            // being blocked from here and the host being switched off look
            // identical on a card and want opposite responses.
            Ok(Err(e)) => {
                debug!("Probe failed: {e}");
                ProbeOutcome::Unreachable { why: format!("{e}") }
            }
            Err(_) => ProbeOutcome::Unreachable {
                why: format!("it did not answer within {}s", timeout.as_secs()),
            },
        }
    }

    async fn try_probe(&self, code: &ConnectionCode) -> Result<ProbeOutcome> {
        let public_key =
            iroh::PublicKey::from_bytes(&code.endpoint_id).context("bad endpoint id")?;

        // # Reach the host the way a real connection would
        //
        // This dialled the bare public key, which leaves iroh nothing to work
        // with but discovery — and discovery means resolving `dns.iroh.link`.
        // On a filtered connection that never answers: twenty-three consecutive
        // failures in the log from the machine that reported this, while the
        // Connect button on the very same host worked every time.
        //
        // So every card said offline and the list was worse than useless — it
        // was confidently wrong, and the one thing it exists to answer is
        // exactly the thing it got wrong.
        //
        // The relay is carried in the code for precisely this reason (see
        // `connection_code`), and it costs nothing to use it here as well.
        let mut addr = iroh::EndpointAddr::new(public_key);
        if let Some(url) = code.relay_url.as_deref().filter(|url| !url.is_empty()) {
            match url.parse() {
                Ok(parsed) => addr = addr.with_relay_url(parsed),
                // A code from a newer or hand-edited build. Discovery is still
                // there to fall back on, so this is not a refusal.
                Err(e) => debug!("Ignoring the relay in a saved code while probing: {e}"),
            }
        }

        // # And nothing else
        //
        // This used to also insert the address the host answered on last time,
        // to match what the Connect button did. Both are gone: an address from
        // a previous session is not evidence about the present, and here it was
        // worse than in the dial loop — inserted unconditionally and first,
        // with no lookup given a chance ahead of it. A sweep that clung to a
        // stale address reported a reachable host as offline, which is the one
        // thing a card exists to answer.
        //
        // What is left is what can actually say where the host is now: the
        // relay in the code, and the lookups attached to this endpoint.

        let conn = self
            .endpoint
            .connect(addr, ALPN)
            .await
            // Named in the same vocabulary the Connect button uses, so a card
            // and a session window explain a failure the same way. Always the
            // relay step now: with no saved address there is no other way this
            // can have been reached, and there is nothing left for `Direct` to
            // describe.
            .with_context(|| DialStep::ViaRelay.stalled_at().to_string())?;

        let (mut send, mut recv) = conn.open_bi().await?;
        write_stream_message(
            &mut send,
            &Message::Probe(super::protocol::Probe { passcode: code.encode() }),
        )
        .await?;

        let Some(bytes) = read_stream_message(&mut recv).await? else {
            anyhow::bail!("host closed without answering the probe");
        };
        let outcome = match bincode::deserialize::<Message>(&bytes)? {
            Message::ProbeResult(result) if result.valid => ProbeOutcome::Valid {
                // Sanitised: this string is drawn in our UI and the host chose it.
                host_name: result.host_name.as_deref().and_then(super::protocol::sanitize_device_name),
                needs_approval: result.needs_approval,
            },
            Message::ProbeResult(_) => ProbeOutcome::Stale,
            // An older host has no idea what a probe is and will close or
            // complain. Either way we reached it, but learned nothing about the
            // code — treating that as "stale" would wrongly grey out a row that
            // still works.
            other => {
                debug!("Host answered a probe with {other:?}");
                ProbeOutcome::Unreachable {
                    why: "it answered with something this version does not understand — it is \
                          probably running an older build"
                        .into(),
                }
            }
        };

        let _ = send.finish();
        conn.close(0u32.into(), b"probe done");
        Ok(outcome)
    }

    pub async fn shutdown(&self) {
        self.endpoint.close().await;
    }
}

// ── Utility functions for reading/writing bincode messages ───

/// Write a length-prefixed, bincode-serialized Message to an async writer.
pub async fn write_stream_message<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<()> {
    let data = bincode::serialize(msg)?;
    let len = u32::try_from(data.len()).context("Message exceeds u32 length prefix")?;

    if data.len() > super::protocol::MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "Refusing to send {} byte message (max {})",
            data.len(),
            super::protocol::MAX_MESSAGE_SIZE
        );
    }

    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&data).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed, bincode-serialized Message from an async reader.
///
/// Returns `Ok(None)` on a clean end-of-stream. **Not cancel-safe** — dropping
/// the returned future mid-read desynchronises the stream.
pub async fn read_stream_message<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<Bytes>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > super::protocol::MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "Message too large: {} bytes (max {})",
            len,
            super::protocol::MAX_MESSAGE_SIZE
        );
    }

    let mut data = vec![0u8; len];
    match reader.read_exact(&mut data).await {
        Ok(_) => Ok(Some(Bytes::from(data))),
        // A truncated body is a broken peer, not a clean shutdown.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            anyhow::bail!("Stream truncated mid-message (expected {} bytes)", len)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DisplayConfig, InputEvent, MouseEvent};
    use tokio::io::duplex;

    fn endpoint(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    /// Two dials from this machine arrive as the same machine.
    ///
    /// # The bug this exists for
    ///
    /// Trusting an admin device remembers the endpoint id its connection
    /// arrived under. That could never match a second time, because this side
    /// bound with no key and iroh mints a fresh keypair for an endpoint that is
    /// not given one — so every connection from one laptop looked like a
    /// different laptop, and the owner was asked to approve their own machine
    /// every single time.
    ///
    /// Checked by building two endpoints rather than by inspecting the builder:
    /// what matters is the id that reaches the far end, and that is a property
    /// of the bound endpoint.
    #[tokio::test]
    async fn every_dial_from_this_machine_carries_the_same_identity() {
        let secret = [0x5Au8; 32];
        adopt_identity(secret);
        assert!(has_identity(), "the identity was not taken up");

        let hosting = *iroh::SecretKey::from_bytes(&secret).public().as_bytes();
        let dialling = outgoing_key().expect("an adopted identity yields a dialling key");

        // Two separate binds, as two separate connection attempts would be.
        let (first, _a) = super::super::discovery::bind(outgoing_builder())
            .await
            .expect("bind an endpoint");
        let (second, _b) = super::super::discovery::bind(outgoing_builder())
            .await
            .expect("bind a second endpoint");

        assert_eq!(
            *first.id().as_bytes(),
            *dialling.public().as_bytes(),
            "a dial did not carry the identity this machine adopted"
        );
        assert_eq!(
            *first.id().as_bytes(),
            *second.id().as_bytes(),
            "two dials from one machine looked like two machines — this is the bug"
        );

        // # And it is not the key this machine *hosts* on
        //
        // Two endpoints under one id is not a thing iroh supports: the relay
        // allows one connection per id and boots the other, so dialling out
        // knocked this machine's own host off the relay. See `outgoing_key`.
        assert_ne!(
            *first.id().as_bytes(),
            hosting,
            "the client and the host are the same endpoint id — they will fight over the relay"
        );

        first.close().await;
        second.close().await;
    }

    /// Dialling yourself is refused before the budget, not after it.
    ///
    /// # Why this is worth its own test
    ///
    /// The dial retries every failure while time remains, deliberately: on a
    /// filtered link a route that failed this second is not a refusal. But a
    /// dial to our own id is refused by iroh instantly and permanently, and the
    /// loop turned that into twenty-four identical refusals over forty-five
    /// seconds — then the outer retry ran it twice more. Two and a half minutes
    /// of "looking for the host" for something knowable before the first
    /// attempt.
    #[tokio::test]
    async fn dialling_our_own_endpoint_fails_at_once() {
        // The endpoint every dial from this process goes out of, asked for its
        // own id — which is what pasting this machine's own code amounts to
        // when the host and the client share one key.
        let (endpoint, _) = outgoing_endpoint().await.expect("bind the outgoing endpoint");
        let ourselves = endpoint.id();

        let started = std::time::Instant::now();
        let outcome = GuestClient::connect_via(&ourselves, None).await;
        let spent = started.elapsed();

        assert!(outcome.is_err(), "dialling ourselves cannot succeed");
        // Comfortably inside the dial's own find budget, which is 45 seconds and
        // is what this used to spend on exactly this answer. Not asserted in
        // microseconds: the check is before the loop, but binding the shared
        // outgoing endpoint happens on the way in and is real work.
        assert!(
            spent < std::time::Duration::from_secs(10),
            "took {spent:?} to refuse a dial that can never work"
        );
    }

    /// Adopting twice keeps the first, because an id that moved mid-run would
    /// be a second machine to every host this process has talked to.
    #[test]
    fn the_identity_this_machine_dials_under_does_not_move() {
        adopt_identity([0x5Au8; 32]);
        let settled = *OUTGOING_IDENTITY.get().expect("an identity");
        adopt_identity([0x99u8; 32]);
        assert_eq!(
            *OUTGOING_IDENTITY.get().expect("an identity"),
            settled,
            "the second call replaced the first"
        );
    }

    // ── Following a shared window ────────────────────────────

    /// A window in a given state, named as a real one would be.
    fn window_in(state: super::super::protocol::SharedWindowState) -> super::super::protocol::SharedWindow {
        super::super::protocol::SharedWindow {
            id: 0x2A,
            title: "Budget 2026.xlsx".into(),
            app: "Excel".into(),
            state,
        }
    }

    fn watching(state: super::super::protocol::SharedWindowState) -> SharedWindowWatch {
        SharedWindowWatch { id: 0x2A, told: window_in(state), owed: false, restored: false }
    }

    /// Nothing is sent while nothing changes.
    ///
    /// This runs on every capture tick — sixty times a second on a live session
    /// — and a message per tick would be a message per frame for a fact that
    /// changes twice an hour.
    #[test]
    fn a_window_that_is_doing_what_it_was_says_nothing() {
        use super::super::protocol::SharedWindowState::*;
        for state in [Showing, Minimised, Closed] {
            let mut watch = watching(state);
            assert!(
                matches!(watch.settle(window_in(state)), WindowStep::Unchanged),
                "{state:?} re-reported itself"
            );
        }
    }

    /// Minimising is announced, and stops the frames.
    #[test]
    fn minimising_is_announced_and_ends_the_stream() {
        use super::super::protocol::SharedWindowState::*;
        let mut watch = watching(Showing);
        assert!(watch.live());

        let step = watch.settle(window_in(Minimised));
        let WindowStep::Announce(said) = step else {
            panic!("the guest was not told the window went away");
        };
        assert_eq!(said.state, Minimised);
        assert_eq!(said.app, "Excel", "the guest needs a name to put in the sentence");
        assert!(!watch.live(), "a minimised window produces no frames");
        assert!(!watch.needs_rebuild(), "nothing to rebuild while it is still away");
    }

    /// Coming back is the one change the capture loop has to act on.
    #[test]
    fn restoring_a_window_asks_for_the_capture_to_be_built_again() {
        use super::super::protocol::SharedWindowState::*;
        let mut watch = watching(Minimised);

        assert!(matches!(watch.settle(window_in(Showing)), WindowStep::Announce(_)));
        assert!(watch.live());
        assert!(
            watch.needs_rebuild(),
            "the capturer was dropped while it was away, so frames need a new one"
        );

        watch.rebuilt();
        assert!(!watch.needs_rebuild(), "asking twice would rebuild it every tick");
    }

    /// A retitle is worth telling the guest and is not worth rebuilding for.
    ///
    /// Browsers rename their window on every navigation, so this is the common
    /// change — and treating it as a restore would tear the capture down and
    /// build it again on every page load.
    #[test]
    fn a_window_that_only_changed_its_title_keeps_its_capture() {
        use super::super::protocol::SharedWindowState::Showing;
        let mut watch = watching(Showing);

        let renamed = super::super::protocol::SharedWindow {
            title: "Forecast.xlsx".into(),
            ..window_in(Showing)
        };
        let WindowStep::Announce(said) = watch.settle(renamed) else {
            panic!("a retitle should reach the guest — it is what the toolbar shows");
        };
        assert_eq!(said.title, "Forecast.xlsx");
        assert!(!watch.needs_rebuild(), "a retitle must not tear the capture down");
        assert!(watch.live());
    }

    /// A window that closes stays closed, and nothing waits for it.
    #[test]
    fn a_closed_window_is_not_waited_for() {
        use super::super::protocol::SharedWindowState::*;
        let mut watch = watching(Showing);
        assert!(matches!(watch.settle(window_in(Closed)), WindowStep::Announce(_)));
        assert!(!watch.live());
        assert!(!watch.needs_rebuild());

        // And it keeps its name, so the guest can say *what* closed.
        assert_eq!(watch.told.app, "Excel");
    }

    /// Switching between two granted windows tells the guest which one it got.
    ///
    /// # Why this needs saying out loud
    ///
    /// The watch reports *changes*, measured against the last thing it read.
    /// Point a fresh one at a second window that happens to be doing the same
    /// thing as the first — both showing, which is the ordinary case — and
    /// nothing has changed, so on the old rule nothing was sent. The guest went
    /// on naming the window it had left, and its switcher showed a selection
    /// that had never been confirmed.
    ///
    /// A switch is news even when the state is not.
    #[test]
    fn switching_to_another_window_is_announced_even_when_nothing_changed() {
        use super::super::protocol::SharedWindowState::Showing;
        use super::super::screen_capture::DisplayTarget;

        // What a session starts with: `SessionStart` already carried this, so
        // an unchanged reading is silence.
        let mut opened = watching(Showing);
        assert!(matches!(opened.settle(window_in(Showing)), WindowStep::Unchanged));

        // What replaces it when the guest picks a different window.
        let mut switched = SharedWindowWatch {
            owed: true,
            ..watching(Showing)
        };
        let WindowStep::Announce(said) = switched.settle(window_in(Showing)) else {
            panic!("a guest that asked for a different window has to be told it got one");
        };
        assert_eq!(said.app, "Excel");
        assert!(
            !switched.needs_rebuild(),
            "the capture for the new window was just built; rebuilding it would drop the \
             first frame"
        );

        // Once said, it is not said again every tick.
        assert!(matches!(switched.settle(window_in(Showing)), WindowStep::Unchanged));

        // And the two constructors differ in exactly that.
        let target = DisplayTarget::Window(0x2A);
        assert!(!SharedWindowWatch::for_target(&target).expect("a window watch").owed);
        assert!(SharedWindowWatch::switched_to(&target).expect("a window watch").owed);
    }

    /// The guest is told which shared windows are open, once per change.
    ///
    /// # Why "once" is half the requirement
    ///
    /// This is asked on every capture tick — sixty times a second — and the
    /// answer changes twice an hour. A message per tick would be a message per
    /// frame carrying a fact nobody asked about, on the same connection as the
    /// video.
    #[test]
    fn the_offer_is_announced_when_it_changes_and_not_otherwise() {
        use super::super::protocol::DisplayGrant;

        // A screen session has none of this to do.
        assert!(Offer::for_grant(&DisplayGrant::All).is_none());
        assert!(Offer::for_grant(&DisplayGrant::Only(vec![0])).is_none());
        assert!(Offer::for_grant(&DisplayGrant::none()).is_none());

        // Handles that never existed, so none of them are ever open — which is
        // the "every shared window has been closed" case, reached without
        // needing a window to close.
        let mut offer = Offer::for_grant(&DisplayGrant::Windows(vec![0x11, 0x22]))
            .expect("a window grant is watched");
        assert_eq!(offer.granted.len(), 2, "the grant is remembered whole");
        assert!(offer.told.is_empty(), "none of those handles are open");

        // Seeded from what `SessionStart` already said, so an unchanged offer
        // says nothing on the first tick or on any after it.
        assert!(offer.moved().is_none(), "an offer that has not moved was announced anyway");
        assert!(offer.moved().is_none());

        // And a grant of real windows starts out knowing they are open, so it
        // does not announce them either.
        let open: Vec<u64> = super::super::window_capture::shareable()
            .iter()
            .take(2)
            .map(|w| w.id)
            .collect();
        if !open.is_empty() {
            let mut offer =
                Offer::for_grant(&DisplayGrant::Windows(open.clone())).expect("watched");
            assert_eq!(
                offer.told.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                open,
                "windows that are open start out offered"
            );
            assert!(offer.moved().is_none(), "nothing changed, so nothing to say");

            // Now pretend the guest was told something else. The next ask
            // reports the truth, once.
            offer.told.clear();
            let announced = offer.moved().expect("the offer changed and was not announced");
            assert_eq!(announced.len(), open.len());
            assert!(offer.moved().is_none(), "announced twice for one change");

            // # A window merely going away still moves the offer
            //
            // It keeps its place in the list — it is still granted and can come
            // back — so an offer compared by id alone would not move, and a
            // composite of nothing but minimised windows would look exactly
            // like a working one.
            let away: Vec<_> = offer
                .told
                .iter()
                .map(|(id, _)| (*id, super::super::protocol::SharedWindowState::Minimised))
                .collect();
            offer.told = away;
            assert!(
                offer.moved().is_some(),
                "a window that came back was not announced, because only the ids were compared"
            );
        }
    }

    /// A source with nothing to give never holds up a request for another one.
    ///
    /// # The freeze this is for
    ///
    /// Close one of several shared windows, open the tab that says so, then ask
    /// for the composite again: the loop was parked on a watch for a window
    /// that had gone, and the display switch sits below that point. The stream
    /// froze and stopped answering every further switch.
    ///
    /// The rule is that parking is an optimisation for having nothing to do,
    /// and "somebody asked for a different picture" is something to do.
    #[test]
    fn a_dead_window_does_not_hold_up_a_switch_to_something_else() {
        use super::super::screen_capture::DisplayTarget;

        let closed = DisplayTarget::Window(0x2A);
        let composite = DisplayTarget::Windows(vec![0x2A, 0x3B]);

        // Nothing to capture and nothing else asked for: park, which is what
        // stops a session on a minimised window burning a core.
        assert!(should_park(false, &closed, &closed));

        // Nothing to capture, and a different picture asked for. This is the
        // one that froze.
        assert!(
            !should_park(false, &composite, &closed),
            "a request to show something else was swallowed by a window that had gone"
        );
        assert!(!should_park(false, &DisplayTarget::Window(0x3B), &closed));

        // And a live window never parks, whatever is asked for — the ordinary
        // path, where the loop goes round and captures a frame.
        assert!(!should_park(true, &closed, &closed));
        assert!(!should_park(true, &composite, &closed));
    }

    /// Only a screen session skips all of this.
    #[test]
    fn a_screen_session_never_watches_a_window() {
        use super::super::screen_capture::DisplayTarget;
        assert!(SharedWindowWatch::for_target(&DisplayTarget::Primary).is_none());
        assert!(SharedWindowWatch::for_target(&DisplayTarget::All).is_none());
        assert!(
            SharedWindowWatch::for_target(&DisplayTarget::One(r"\\.\DISPLAY1".into())).is_none()
        );
        // And a window session does, whatever state the window is in — the
        // handle is all this needs to start following it.
        assert!(SharedWindowWatch::for_target(&DisplayTarget::Window(0x2A)).is_some());
    }

    /// What the guest is told it is watching, for each way of capturing.
    #[test]
    fn what_a_session_is_showing_is_said_in_the_guests_terms() {
        use super::super::protocol::{DisplayChoice, DisplayInfo};
        use super::super::screen_capture::DisplayTarget;

        let screen = DisplayInfo {
            id: 3,
            name: r"\\.\DISPLAY2".into(),
            monitor: None,
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            primary: true,
            hdr: false,
            virtual_display: false,
        };
        let available = [screen.clone()];

        assert_eq!(showing_now(&DisplayTarget::All, &available), DisplayChoice::All);
        assert_eq!(
            showing_now(&DisplayTarget::Window(9), &available),
            DisplayChoice::Window(9)
        );
        assert_eq!(
            showing_now(&DisplayTarget::One(r"\\.\DISPLAY2".into()), &available),
            DisplayChoice::One(3),
            "a device name has to come back as the id the guest was given"
        );
        assert_eq!(showing_now(&DisplayTarget::Primary, &available), DisplayChoice::One(3));
    }

    /// A guest is asked about every time, whatever else is remembered.
    ///
    /// The trusted set is about admin endpoints and must not leak into the
    /// guest rule — an endpoint approved once as the owner turning up later
    /// with a guest code is still a guest, and what a guest may do is decided
    /// per connection.
    #[test]
    fn a_guest_is_always_asked_about() {
        let mut trusted = std::collections::BTreeSet::new();
        trusted.insert(endpoint(1));

        assert!(must_be_approved(PasscodeType::Guest, &endpoint(1), &trusted));
        assert!(must_be_approved(PasscodeType::Guest, &endpoint(2), &trusted));
        assert!(must_be_approved(PasscodeType::Guest, &endpoint(1), &Default::default()));
    }

    /// An admin is asked about once per endpoint, and then not again.
    #[test]
    fn an_admin_is_asked_about_until_the_host_says_yes() {
        let mut trusted = std::collections::BTreeSet::new();

        // Nobody is trusted to begin with, so the first admin connection from
        // anywhere has to be answered.
        assert!(
            must_be_approved(PasscodeType::Master, &endpoint(1), &trusted),
            "an unknown endpoint holding the admin code must not walk in"
        );

        trusted.insert(endpoint(1));
        assert!(
            !must_be_approved(PasscodeType::Master, &endpoint(1), &trusted),
            "the owner should not be asked again from a device they already approved"
        );
        // And approving one device says nothing about any other.
        assert!(
            must_be_approved(PasscodeType::Master, &endpoint(2), &trusted),
            "trust is per endpoint — a second device is a second decision"
        );
    }

    /// The whole point of the feature, stated as the thing it prevents.
    ///
    /// Somebody who has come by the admin passcode — pasted into the wrong
    /// window, read off a screen share — still cannot use it without somebody
    /// at the machine saying yes. The code alone stopped being sufficient, and
    /// this is what says so.
    #[test]
    fn a_leaked_admin_code_alone_does_not_open_this_host() {
        let owner = endpoint(7);
        let thief = endpoint(200);
        let trusted = std::collections::BTreeSet::from([owner]);

        assert!(
            must_be_approved(PasscodeType::Master, &thief, &trusted),
            "holding the code must not be the whole of the authorisation"
        );
    }

    /// Resetting the passcode is the revocation, so it must take trust with it.
    ///
    /// Modelled here as what the reset does — empty the set — because that is
    /// the operation the interface offers, and a reset that left a leaked
    /// machine trusted would look like it had worked and would not have.
    #[test]
    fn clearing_the_trusted_set_locks_everyone_out_again() {
        let trusted = std::collections::BTreeSet::from([endpoint(1), endpoint(2)]);
        assert!(!must_be_approved(PasscodeType::Master, &endpoint(1), &trusted));

        let after_reset = std::collections::BTreeSet::new();
        for known in [endpoint(1), endpoint(2)] {
            assert!(
                must_be_approved(PasscodeType::Master, &known, &after_reset),
                "a passcode reset has to take its trusted endpoints with it"
            );
        }
    }

    /// A default host trusts nobody.
    #[test]
    fn nothing_is_trusted_until_it_is_said_so() {
        assert!(
            SessionConfig::default().trusted_admins.is_empty(),
            "a default that trusted anything would make the check decorative"
        );
    }

    /// Every dial step must say something different, in both directions.
    ///
    /// The whole value of reporting steps is that the one left on screen
    /// identifies what failed. Two steps sharing a phrase would silently undo
    /// that — the user would see a sentence that could mean either, which is
    /// where "Connecting…" started.
    ///
    /// The stalled text is held to more than being distinct: it has to name the
    /// thing that did not happen, so it is checked for being a sentence rather
    /// than a label. A step whose failure text reads like a status is not
    /// telling anybody what to do next.
    #[test]
    fn each_dial_step_says_something_of_its_own() {
        let steps = [
            DialStep::Starting,
            DialStep::ReachingRelay,
            DialStep::ViaRelay,
            DialStep::Direct,
            DialStep::Punching,
            DialStep::Authenticating,
        ];

        let mut labels = std::collections::HashSet::new();
        let mut stalls = std::collections::HashSet::new();
        for step in steps {
            assert!(labels.insert(step.label()), "{step:?} shares its label with another step");
            assert!(
                stalls.insert(step.stalled_at()),
                "{step:?} shares its failure text with another step"
            );
            assert!(
                step.stalled_at().len() > step.label().len(),
                "{step:?} explains its failure in fewer words than it names itself, which \
                 means it is not explaining"
            );
            assert!(
                !step.stalled_at().ends_with('…') && !step.stalled_at().ends_with("..."),
                "{step:?} reads like something still in progress rather than something that \
                 stopped"
            );
        }
    }

    /// A still desktop is not a broken link.
    ///
    /// # The bug this is for
    ///
    /// `acks_stale` was silence alone, and silence is what a healthy connection
    /// sounds like when the host has nothing to send. A static screen produces
    /// no frames — the capture path counts "nothing changed" separately for
    /// exactly this reason — so nothing went out, nothing came back, and five
    /// seconds later the controller was told the link had stopped carrying.
    ///
    /// What that looked like: a resolution falling while the encoder sat idle
    /// and the round trip read three milliseconds, with "Slow network" on a
    /// stream doing 0 kbit/s. And worst on the screens the feature is most
    /// wanted for — a virtual display or a dummy plug showing a still picture
    /// sends nothing for minutes at a time.
    #[test]
    fn silence_is_only_evidence_when_something_was_sent() {
        // Everything acknowledged, nothing since: the ordinary still desktop.
        assert!(
            !acks_are_stale(0, true, false),
            "a link with nothing outstanding cannot be failing to carry it"
        );

        // A frame is out there and has gone unacknowledged. The real signal.
        assert!(acks_are_stale(1, true, false));

        // Still flowing.
        assert!(!acks_are_stale(1, true, true));

        // A session in its first seconds has acknowledged nothing yet, which is
        // a stream starting rather than one that stopped — the fault this
        // guard was added for, and it still holds.
        assert!(!acks_are_stale(5, false, false));
    }

    fn test_config() -> SessionConfig {
        SessionConfig {
            capture: CaptureConfig {
                display: Default::default(),
                fps: 120,
                quality: QualityScale::Native,
                // 32x16 rather than 8x4: H.264 is the default codec and needs
                // more than half a macroblock to work with.
                show_cursor: false,
                native_override: Some((32, 16)),
            },
            encode: EncodeConfig::default(),
            exclude_audio_processes: Vec::new(),
            audio_bitrate_kbps: 160,
            approval_timeout: Duration::from_millis(50),
            auto_regen_guest_passcode: false,
            host_name: None,
            priority: crate::LatencyPriority::default(),
            // Off in tests: these run against a synthetic capture with no
            // display behind it, and a test that resized the developer's
            // desktop would be a memorable way to find that out.
            virtual_display_follows_guest: false,
            // Empty, so these exercise a host that has approved nobody — which
            // is the state every host starts in.
            trusted_admins: Default::default(),
            // Off in tests: there is no person at this machine to give way to,
            // and a developer's own mouse moving past would otherwise start
            // dropping the input a test had just sent.
            local_input_wins_for: Duration::ZERO,
        }
    }

    /// Drive `run_session` over an in-memory duplex pair, returning the guest's ends.
    fn spawn_session(
        sessions: Arc<SessionManager>,
        session_id: u32,
    ) -> (
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        // host_w → guest_r (frames), guest_w → host_r (input)
        let (host_w, guest_r) = duplex(1 << 20);
        let (guest_w, host_r) = duplex(1 << 20);

        let config = test_config();
        let capturer = crate::screen_capture::create_capturer(config.capture.clone()).unwrap();
        let handle =
            // Neither side stream exists in the duplex harness: audio and
            // video both fall back to the main one, which is the behaviour
            // being exercised here and the fallback worth keeping covered.
            tokio::spawn(run_session(
                host_w,
                host_r,
                None::<tokio::io::DuplexStream>,
                None::<tokio::io::DuplexStream>,
                session_id,
                sessions,
                config.clone(),
                Arc::new(std::sync::Mutex::new(config)),
                capturer,
                crate::protocol::DisplayGrant::All,
            ));
        (guest_w, guest_r, handle)
    }

    #[tokio::test]
    async fn host_streams_frames_without_the_guest_sending_anything() {
        // Regression: the frame pump used to live inside the read loop, so a silent
        // guest received zero frames.
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::ALL, PasscodeType::Master);
        let (_guest_w, mut guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        // Audio and status share this stream, so "the next message" is not
        // necessarily a frame — only that frames keep coming without being
        // asked for, which is what this is guarding.
        let mut frames = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while frames < 3 {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "only {frames} frames arrived unprompted");
            let bytes = tokio::time::timeout(remaining, read_stream_message(&mut guest_r))
                .await
                .expect("timed out waiting for a frame")
                .unwrap()
                .expect("stream closed early");
            if matches!(bincode::deserialize::<Message>(&bytes).unwrap(), Message::Frame(_)) {
                frames += 1;
            }
        }

        handle.abort();
    }

    #[tokio::test]
    async fn ping_is_answered_with_pong_while_frames_flow() {
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::ALL, PasscodeType::Master);
        let (mut guest_w, mut guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        write_stream_message(&mut guest_w, &Message::Ping(4242)).await.unwrap();

        let got_pong = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let bytes = read_stream_message(&mut guest_r).await.unwrap().unwrap();
                if let Message::Pong { echo: ts, .. } = bincode::deserialize::<Message>(&bytes).unwrap() {
                    return ts;
                }
            }
        })
        .await
        .expect("timed out waiting for pong");

        assert_eq!(got_pong, 4242);
        handle.abort();
    }

    #[tokio::test]
    async fn guest_config_reaches_the_session_table() {
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let (mut guest_w, _guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        write_stream_message(
            &mut guest_w,
            &Message::Config(DisplayConfig {
                target_fps: 30,
                max_bitrate_kbps: 4000,
                client_width: 1280,
                client_height: 720,
            }),
        )
        .await
        .unwrap();

        let cfg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(c) = sessions.get(id).and_then(|s| s.display_config) {
                    return c;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("display config never applied");

        assert_eq!(cfg.client_width, 1280);
        handle.abort();
    }

    #[tokio::test]
    async fn input_is_accepted_only_while_control_is_granted() {
        // Regression: control_granted used to be captured once at connect time, so
        // approving mid-session had no effect.
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let (mut guest_w, _guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        let move_event = Message::Input(InputEvent::MouseMove(MouseEvent { x: 10, y: 20 }));

        // View-only: dropped, and the session must stay alive.
        write_stream_message(&mut guest_w, &move_event).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "dropped input must not kill the session");

        // Granting control mid-session takes effect on the next event.
        assert!(sessions.set_control(id, ControlGrants::ALL));
        write_stream_message(&mut guest_w, &move_event).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished());
        assert!(sessions.has_any_control(id));

        handle.abort();
    }

    #[tokio::test]
    async fn mouse_grant_does_not_admit_keyboard_input() {
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let (mut guest_w, _guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        // Pointer only — the keyboard stays off.
        assert!(sessions.set_control(id, ControlGrants::new(true, false)));

        let grants = sessions.grants(id).unwrap();
        assert!(grants.allows(crate::protocol::InputDevice::Mouse));
        assert!(!grants.allows(crate::protocol::InputDevice::Keyboard));

        // Both event kinds are sent; only the mouse one is eligible for injection,
        // and neither may tear the session down.
        write_stream_message(
            &mut guest_w,
            &Message::Input(InputEvent::MouseMove(MouseEvent { x: 1, y: 2 })),
        )
        .await
        .unwrap();
        write_stream_message(
            &mut guest_w,
            &Message::Input(InputEvent::Key(crate::protocol::KeyEvent {
                scancode: 0,
                extended: false,
                keycode: 65,
                pressed: true,
                modifiers: 0,
            })),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "a dropped keystroke must not kill the session");
        assert_eq!(sessions.grants(id).unwrap(), ControlGrants::new(true, false));

        handle.abort();
    }

    #[tokio::test]
    async fn grant_changes_are_pushed_to_the_guest() {
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let (_guest_w, mut guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        // Give the writer loop a moment to register its outbound channel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(sessions.set_control(id, ControlGrants::new(false, true)));

        let grants = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let bytes = read_stream_message(&mut guest_r).await.unwrap().unwrap();
                if let Message::ControlState(cs) = bincode::deserialize::<Message>(&bytes).unwrap() {
                    return cs.grants;
                }
            }
        })
        .await
        .expect("guest was never told its permissions changed");

        assert_eq!(grants, ControlGrants::new(false, true));
        handle.abort();
    }

    #[tokio::test]
    async fn session_ends_when_the_guest_disconnects() {
        let sessions = Arc::new(SessionManager::new());
        let id = sessions.add_session([0u8; 32], ControlGrants::ALL, PasscodeType::Master);
        let (guest_w, _guest_r, handle) = spawn_session(Arc::clone(&sessions), id);

        drop(guest_w);

        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("session did not end after guest disconnect");
        assert!(outcome.unwrap().is_ok());
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected_before_allocating() {
        let (mut w, mut r) = duplex(64);
        let too_big = (super::super::protocol::MAX_MESSAGE_SIZE as u32) + 1;
        tokio::io::AsyncWriteExt::write_all(&mut w, &too_big.to_be_bytes()).await.unwrap();

        let err = read_stream_message(&mut r).await.unwrap_err();
        assert!(err.to_string().contains("Message too large"));
    }

    #[tokio::test]
    async fn clean_eof_reads_as_none_but_truncation_is_an_error() {
        let (w, mut r) = duplex(64);
        drop(w);
        assert!(read_stream_message(&mut r).await.unwrap().is_none());

        let (mut w2, mut r2) = duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut w2, &16u32.to_be_bytes()).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut w2, b"short").await.unwrap();
        drop(w2);
        assert!(read_stream_message(&mut r2).await.is_err());
    }

    #[tokio::test]
    async fn message_roundtrips_through_the_framing() {
        let (mut w, mut r) = duplex(1 << 16);
        let sent = Message::Auth {
            passcode: "MASTER-abc".into(),
            session_id: 7,
            client_name: Some("Studio PC".into()),
        };
        write_stream_message(&mut w, &sent).await.unwrap();

        let bytes = read_stream_message(&mut r).await.unwrap().unwrap();
        match bincode::deserialize::<Message>(&bytes).unwrap() {
            Message::Auth { passcode, session_id, client_name } => {
                assert_eq!(passcode, "MASTER-abc");
                assert_eq!(session_id, 7);
                assert_eq!(client_name.as_deref(), Some("Studio PC"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn a_guest_that_never_acks_is_never_throttled() {
        // Acks were added after the fact. Starving an older guest — or one whose
        // acks are being lost — of every frame after the first two would be a
        // far worse failure than the backlog the window exists to prevent.
        let state = AckState::default();
        assert!(!state.is_reporting());
    }

    #[test]
    fn a_guest_that_stops_acking_stops_being_believed() {
        let mut state = AckState::default();
        state.record(10);
        assert!(state.is_reporting());

        // Simulate the ack ageing out.
        state.last_at = Some(std::time::Instant::now() - ACK_STALENESS_LIMIT * 2);
        assert!(!state.is_reporting(), "a stale ack must not hold the stream shut");
    }

    #[test]
    fn acks_only_move_forward() {
        // A reordered or duplicated ack must not rewind the window and let the
        // host run further ahead than the guest has actually reached.
        let mut state = AckState::default();
        state.record(50);
        state.record(20);
        assert_eq!(state.sequence, 50);
    }

    /// The window has to admit the rate it was sized for.
    ///
    /// Throughput through the window is `window / ack_latency`. If that comes
    /// out below the rate being attempted, the stream can never reach the plan
    /// however good the link is — it settles wherever the window happens to
    /// pin it.
    #[test]
    fn the_window_admits_the_rate_it_is_sized_for() {
        for fps in [15u32, 30, 60] {
            for ack_ms in [20.0f32, 60.0, 150.0] {
                let window = send_window(ack_ms, fps) as f32;
                let admitted = window / (ack_ms / 1000.0);
                assert!(
                    admitted >= fps as f32 * 0.5,
                    "a window of {window} at {ack_ms} ms admits {admitted:.0} fps, \
                     which cannot sustain a {fps} fps plan",
                );
            }
        }
    }

    /// Why the window is sized from the plan and not from what is achieved.
    ///
    /// Sizing it from the achieved rate does not lock a stream in place — the
    /// window always admits somewhat more than the rate that produced it, so
    /// there is a way up. It is a *drag*, and how heavy depends on the ack
    /// latency: the headroom is one window's worth per acknowledgement, which
    /// shrinks as acknowledgements slow down. On a link where acks take four
    /// hundred milliseconds a stream at 10 fps is allowed twelve, and climbing
    /// back to sixty from there takes longer than anyone watches.
    ///
    /// Kept as arithmetic because the call site reads perfectly reasonably
    /// either way and the difference between them does not show up until
    /// somebody counts frames on a link that is nowhere near saturated.
    #[test]
    fn sizing_the_window_from_the_achieved_rate_drags() {
        // Slow acknowledgements are where the two answers diverge.
        let ack_ms = 400.0;

        let from_achieved = send_window(ack_ms, 10) as f32 / (ack_ms / 1000.0);
        let from_plan = send_window(ack_ms, 60) as f32 / (ack_ms / 1000.0);

        assert!(
            from_plan > from_achieved * 1.5,
            "sized from the plan the window admits {from_plan:.0} fps, from the achieved \
             rate only {from_achieved:.0} — the second is the drag this avoids",
        );
    }

    #[test]
    fn the_send_window_scales_with_the_round_trip() {
        // A fixed frame count would cap a distant guest at one frame per round
        // trip — 5 fps on a 200 ms link — because the frames in flight are
        // mostly propagation delay rather than backlog.
        let near = send_window(10.0, 60);
        let far = send_window(200.0, 60);
        assert!(far > near, "a longer round trip needs a wider window");

        // But never unbounded: this is the latency guarantee.
        assert!(send_window(10_000.0, 60) <= SEND_WINDOW_MAX);
        assert!(send_window(0.0, 60) >= 2, "the window must always admit a frame");
        assert!(send_window(f32::NAN, 60) >= 2, "a nonsense round trip must not close it");
    }

    /// The window is sized from how long an ack really takes, decode included.
    ///
    /// # The session this comes from
    ///
    /// A guest on a relayed link reported decode at 9 to 12 ms a frame, three
    /// quarters of it the readback from its own GPU. It acks a frame once it has
    /// decoded it — so an ack took a round trip *plus* a decode, while the host
    /// sized its window for a round trip alone. The host hit the gate, stalled,
    /// burst when acks arrived, and hit it again: 1 to 11 frames a second held
    /// back, every second, with its own diagnosis flipping between "slow
    /// network" and "running clean" every few seconds.
    #[test]
    fn the_window_covers_the_whole_acknowledgement_not_just_the_round_trip() {
        let mut state = AckState::default();
        state.begin();

        // A frame goes out and is acked 60 ms later — a 40 ms link and a 20 ms
        // decode, which is what a real guest looked like.
        state.sent(1);
        std::thread::sleep(Duration::from_millis(60));
        state.record(1);

        let measured = state.ack_latency_ms().expect("an acked frame must be timed");
        assert!(
            (50.0..90.0).contains(&measured),
            "measured {measured:.0} ms for a 60 ms acknowledgement"
        );
        // And that is what sizes the window, so it is wider than a window sized
        // from the link alone would have been.
        assert!(
            send_window(measured, 60) > send_window(40.0, 60),
            "a slower acknowledgement must buy a wider window: {} against {}",
            send_window(measured, 60),
            send_window(40.0, 60)
        );
    }

    /// An ack for a frame we never recorded must not corrupt the estimate.
    #[test]
    fn an_ack_for_an_unknown_frame_leaves_the_estimate_alone() {
        let mut state = AckState::default();
        state.begin();
        state.sent(10);
        state.record(99);
        assert_eq!(state.ack_latency_ms(), None, "a stray ack must not invent a measurement");
    }

    /// The slack is a duration, so a slow stream does not get a huge one.
    ///
    /// It used to be two frames flat. At 8 fps — where a struggling stream ends
    /// up — that is a quarter of a second of video queued on top of the round
    /// trip, and it was most of the 433 ms a real long-distance session measured
    /// end to end over a 102 ms link.
    #[test]
    fn the_windows_slack_is_bounded_in_time_not_in_frames() {
        let queued_ms = |fps: u32| {
            let frames = send_window(100.0, fps) as f32;
            frames * 1000.0 / fps as f32
        };

        let fast = queued_ms(60);
        let slow = queued_ms(8);
        assert!(
            slow <= fast * 2.0,
            "a slow stream queues {slow:.0} ms against a fast one's {fast:.0} ms"
        );
        // And the old behaviour — two frames of slack at 8 fps — is gone.
        assert!(send_window(100.0, 8) < 3, "8 fps should not carry two frames of slack");
    }

    #[test]
    fn denial_and_view_only_approval_are_different_answers() {
        // "Allowed, but no input" is not the same as "refused" — the first one
        // still streams the screen.
        assert_eq!(ApprovalDecision::Allow { grants: ControlGrants::NONE, displays: crate::protocol::DisplayGrant::All }.grants(), ControlGrants::NONE);
        assert_eq!(ApprovalDecision::Deny.grants(), ControlGrants::NONE);
        assert_ne!(ApprovalDecision::Allow { grants: ControlGrants::NONE, displays: crate::protocol::DisplayGrant::All }, ApprovalDecision::Deny);
        assert_eq!(ApprovalDecision::Allow { grants: ControlGrants::ALL, displays: crate::protocol::DisplayGrant::All }.grants(), ControlGrants::ALL);
    }

}
