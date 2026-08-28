//! iroh-based P2P networking layer.
//!
//! Provides `HostServer` (accepts incoming connections, manages sessions) and
//! `GuestClient` (connects to a host via passcode-protected ticket).
//!
//! The ALPN protocol name is `"tain/2"` — every connection carrying this ALPN
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
pub const ALPN: &[u8] = b"tain/2";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Stream the desktop. `grants` says which input devices may be driven —
    /// [`ControlGrants::NONE`] is a legitimate answer, meaning view-only.
    Allow(ControlGrants),
    /// Refuse: the guest is disconnected without ever seeing a frame.
    Deny,
}

impl ApprovalDecision {
    /// Grants implied by this decision.
    pub fn grants(self) -> ControlGrants {
        match self {
            Self::Allow(grants) => grants,
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
    /// Oneshot sender carrying the host's decision.
    pub approve_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
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
        let mut builder = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .alpns(vec![ALPN.to_vec()])
            // Relay hostnames answered from what was resolved last time, so a
            // link that cannot reach DNS can still reach the relays. See
            // `dns_cache`.
            .dns_resolver(super::dns_cache::resolver());
        if let Some(bytes) = identity {
            builder = builder.secret_key(iroh::SecretKey::from_bytes(&bytes));
        }
        let endpoint = builder.bind().await?;

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

    /// Disconnect a guest from the host side.
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
                    reason: "invalid_passcode".into(),
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
        let label = client_name.clone().unwrap_or_else(|| format!("Guest #{session_id}"));
        self.sessions.set_label(session_id, label.clone());
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

        // Guests wait here. Nothing has been captured, encoded, or sent — a
        // refused guest never sees a pixel of this machine.
        let grants = if typ == PasscodeType::Guest {
            write_stream_message(
                &mut send,
                &Message::GuestApprovalRequest(super::protocol::GuestApprovalRequest {
                    session_id,
                    label: label.clone(),
                }),
            )
            .await?;

            let decision = self
                .await_approval(session_id, label, client_name.is_some(), config.approval_timeout)
                .await;

            match decision {
                ApprovalDecision::Allow(grants) => {
                    self.sessions.set_control(session_id, grants);
                    grants
                }
                ApprovalDecision::Deny => {
                    info!("Session #{} was refused by the host", session_id);
                    write_stream_message(
                        &mut send,
                        &Message::SessionEnd(super::protocol::SessionEnd {
                            session_id,
                            reason: "the host did not allow this connection".into(),
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
                write_stream_message(
                    &mut send,
                    &Message::SessionEnd(super::protocol::SessionEnd {
                        session_id,
                        reason: format!("the host could not capture its screen: {e}"),
                    }),
                )
                .await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(5), send.stopped()).await;
                return Ok(());
            }
        };
        let (capture_w, capture_h) = capturer.frame_size();
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
        )
        .await
    }

    /// Ask the host to approve a guest and wait for the answer.
    ///
    /// Every path that is not an explicit "allow" denies. A host that cannot be
    /// asked — no UI attached, receiver gone, nobody at the keyboard — must not
    /// end up streaming its screen by default.
    async fn await_approval(
        self: &Arc<Self>,
        session_id: u32,
        label: String,
        label_is_self_reported: bool,
        timeout: Duration,
    ) -> ApprovalDecision {
        let Some(ref tx) = self.approval_tx else {
            warn!(
                "Session #{}: no approval channel attached — refusing the guest",
                session_id
            );
            return ApprovalDecision::Deny;
        };

        let (approve_tx, approve_rx) = tokio::sync::oneshot::channel();
        let req = ApprovalRequest { session_id, label, label_is_self_reported, approve_tx };
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
    let mut reader = tokio::spawn(run_reader(
        recv,
        session_id,
        Arc::clone(&sessions),
        out_tx.clone(),
        Arc::clone(&telemetry),
        Arc::clone(&acks),
        Arc::clone(&viewport),
        Arc::clone(&keyframe_wanted),
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
                    if keyframe_wanted.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        encoder.request_keyframe();
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
                let acks_stale = {
                    let state = acks.lock().unwrap();
                    state.has_ever_acked() && !state.is_reporting()
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

/// Grab frames at the requested rate and hand them to the encoder.
///
/// Separate from encoding so the two **overlap**. They used to run in sequence
/// on one thread, which meant the frame rate was bounded by their sum: measured
/// at 5.1 ms capture plus 5.4 ms encode, or 10.5 ms, against an 8.3 ms budget at
/// 120 fps. Overlapping bounds it by the slower of the two instead. Per-frame
/// latency is unchanged — a frame still waits for its own capture and its own
/// encode — so this buys throughput without costing responsiveness.
fn spawn_capture_thread(
    session_id: u32,
    capturer: Box<dyn super::screen_capture::ScreenCaptureBackend>,
    tx: std::sync::mpsc::SyncSender<ScreenFrame>,
    plan: CapturePlan,
    viewport: SharedViewport,
    cursor_tx: mpsc::UnboundedSender<super::cursor::CursorUpdate>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("tain-capture-{session_id}"))
        .spawn(move || {
            let mut capturer = capturer;
            let mut applied_scale = plan.lock().unwrap().0;
            let mut applied_ceiling: Option<(u16, u16)> = None;
            let mut next_frame_at = std::time::Instant::now();
            let (mut produced, mut unchanged, mut dropped) = (0u32, 0u32, 0u32);
            let mut produced_at = std::time::Instant::now();

            loop {
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
) where
    R: tokio::io::AsyncReadExt + Unpin + Send + 'static,
{
    let injector = super::input_inject::create_injector();
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
                sessions.set_display_config(session_id, cfg);
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
                *telemetry.lock().unwrap() = Some(sanitize_telemetry(report));
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
    endpoint: iroh::Endpoint,
    connection: Option<iroh::endpoint::Connection>,
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
    /// Connected at the transport level; sending the passcode.
    Authenticating,
}

impl DialStep {
    /// What to show while this step is the one in progress.
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting up…",
            Self::ReachingRelay => "Looking up where the host is…",
            Self::ViaRelay => "Reaching the host through its relay…",
            Self::Direct => "Reaching the host directly…",
            Self::Authenticating => "Sending the passcode…",
        }
    }

    /// What to say when the attempt gave up here.
    ///
    /// Phrased as the thing that did not happen rather than as an error code,
    /// because the useful next action differs at every step.
    pub fn stalled_at(self) -> &'static str {
        match self {
            Self::Starting => "this machine could not open a connection of its own",
            Self::ReachingRelay => {
                "no relay could be reached from here, and the code carries no address to \
                 try directly — ask for a fresh code, or connect once on a network that \
                 works so an address can be remembered"
            }
            Self::ViaRelay => {
                "the host's relay could not be reached from here. The host may be fine and \
                 the relay blocked"
            }
            Self::Direct => {
                "the host did not answer on the address it last used. It may be switched \
                 off, asleep, or on another network"
            }
            Self::Authenticating => "the host answered but did not accept the passcode",
        }
    }
}

impl GuestClient {
    /// Connect to the host identified by its PublicKey (EndpointId).
    pub async fn connect(host_public_key: &iroh::PublicKey) -> Result<Self> {
        Self::connect_via(host_public_key, None, &[]).await
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
        known_addrs: &[std::net::SocketAddr],
    ) -> Result<Self> {
        Self::connect_reporting(host_public_key, relay_url, known_addrs, &|_| {}).await
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
        known_addrs: &[std::net::SocketAddr],
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

        /// How long one dial may take before the host is called unreachable.
        ///
        /// Generous for the same reason the paste check is: a cold path may
        /// need a relay handshake and hole punching first, and iroh retries
        /// inside this with a growing backoff. But it is bounded, because the
        /// alternative is a session that sits on "Connecting" with nothing to
        /// report and nothing to cancel.
        const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

        // The cached resolver, as everywhere else — this is the path a user on
        // a censored link actually presses, and the one where three seconds of
        // DNS per relay is the difference between connecting and not.
        on_step(DialStep::Starting);
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .dns_resolver(super::dns_cache::resolver())
            .bind()
            .await?;

        // Only worth naming as a step when there is nothing else to go on. With
        // a relay in the code or an address on file, this wait is a courtesy
        // and the user should not be told the connection depends on it.
        if relay_url.is_none_or(str::is_empty) && known_addrs.is_empty() {
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

        // # Where this host answered last time
        //
        // An address that worked once is worth trying again, and trying it
        // costs nothing: iroh races every candidate it is given and keeps
        // whichever answers first. A stale one loses the race and is forgotten;
        // it cannot reach the wrong machine, because the endpoint id is a
        // public key and the handshake checks it.
        //
        // What it buys is a connection that needs no relay at all. On a link
        // where the relays are unreachable — which is the whole reason there is
        // a tunnel in the first place — that is the difference between
        // connecting and not.
        for known in known_addrs {
            addr.addrs.insert(iroh::TransportAddr::Ip(*known));
        }
        if !known_addrs.is_empty() {
            info!("Also trying {} address(es) this host answered on before", known_addrs.len());
        }

        // Whichever way this is going to be reached, said before the wait
        // rather than after it — a step nobody sees is not a step.
        on_step(if known_addrs.is_empty() { DialStep::ViaRelay } else { DialStep::Direct });

        let conn = match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr.clone(), ALPN))
            .await
        {
            Ok(Ok(conn)) => conn,
            // A dial that failed while this endpoint had not finished coming
            // online is the one case worth repeating: the first attempt had no
            // relay to fall back on and the second may. Only then, and only
            // once — retrying a refusal that had every chance of working turns
            // a clear failure into a slow one.
            Ok(Err(first)) if !online => {
                warn!("First attempt failed before this endpoint was online ({first}) — retrying");
                let _ = tokio::time::timeout(ONLINE_WAIT, endpoint.online()).await;
                match tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(addr, ALPN)).await {
                    Ok(result) => result.context("Failed to connect to host")?,
                    Err(_) => anyhow::bail!(
                        "That host did not answer within {DIAL_TIMEOUT:?}. It may be offline, \
                         or the relay in its code may be unreachable from here — ask for a \
                         fresh code if the host has moved network."
                    ),
                }
            }
            Ok(Err(e)) => return Err(e).context("Failed to connect to host"),
            Err(_) => anyhow::bail!(
                "That host did not answer within {DIAL_TIMEOUT:?}. It may be offline, or the \
                 relay in its code may be unreachable from here — ask for a fresh code if the \
                 host has moved network."
            ),
        };
        info!("Connected to host: {}", conn.remote_id());

        Ok(Self { endpoint, connection: Some(conn) })
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
    pub async fn connect_with_code(
        code: &ConnectionCode,
        known_addrs: &[std::net::SocketAddr],
    ) -> Result<Self> {
        Self::connect_with_code_reporting(code, known_addrs, &|_| {}).await
    }

    /// As [`Self::connect_with_code`], saying which step it has reached.
    pub async fn connect_with_code_reporting(
        code: &ConnectionCode,
        known_addrs: &[std::net::SocketAddr],
        on_step: &(dyn Fn(DialStep) + Send + Sync),
    ) -> Result<Self> {
        let public_key =
            iroh::PublicKey::from_bytes(&code.endpoint_id).context("Invalid endpoint ID bytes")?;
        // The relay the host put in the code, so no lookup is needed to find
        // it. Codes issued before this existed carry none, and fall back to the
        // lookup path. `known_addrs` is where this host answered last time,
        // which can remove the relay from the path entirely.
        Self::connect_reporting(&public_key, code.relay_url.as_deref(), known_addrs, on_step).await
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
        let conn = self.connection.as_ref()?;
        let paths = conn.paths();
        // The *selected* path is the one carrying data. A connection commonly
        // holds an idle relay path alongside a direct one, so reporting the
        // first path found would say "relay" for a healthy direct session.
        let selected = paths.iter().find(|p| p.is_selected()).or_else(|| paths.iter().next())?;
        let rtt_ms = selected.rtt().as_secs_f32() * 1000.0;

        Some(if selected.is_relay() {
            ConnectionPath::Relay {
                host: format!("{}", selected.remote_addr()),
                rtt_ms,
            }
        } else {
            // The address is carried for direct paths too, because "direct" only
            // means "not through an iroh relay" — it says nothing about the route
            // those packets then take. Two machines both on a VPN get a direct
            // path *to their VPN addresses*, and the round trip is the tunnel's,
            // which can be several times the physical one. That reads as "direct,
            // 110 ms" with nothing on screen to explain it.
            ConnectionPath::Direct { rtt_ms, remote: format!("{}", selected.remote_addr()) }
        })
    }

    /// Shut down the guest endpoint.
    pub async fn shutdown(&self) -> Result<()> {
        self.endpoint.close().await;
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
            Self::Direct { .. } => "Connected peer to peer.".to_string(),
            Self::Relay { host, .. } => format!(
                "Relayed through {host} because a direct connection could not be established. Relays are shared and rate-limited, so video over one stutters no matter what quality is chosen. A VPN on either machine is a common cause."
            ),
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::ConnectionPath;

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
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .dns_resolver(super::dns_cache::resolver())
            .bind()
            .await?;
        endpoint.online().await;
        Ok(Self { endpoint })
    }

    /// Ask a host whether it is there and whether this code still works.
    ///
    /// Never raises an approval request on the host, and never fails the caller:
    /// every outcome, including "could not reach it", is an answer worth showing
    /// on the card.
    pub async fn probe(
        &self,
        code: &ConnectionCode,
        known_addrs: &[std::net::SocketAddr],
        timeout: Duration,
    ) -> ProbeOutcome {
        match tokio::time::timeout(timeout, self.try_probe(code, known_addrs)).await {
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

    async fn try_probe(
        &self,
        code: &ConnectionCode,
        known_addrs: &[std::net::SocketAddr],
    ) -> Result<ProbeOutcome> {
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

        // # The same addresses the Connect button would use
        //
        // This probed with the relay alone. Connecting also tries wherever the
        // host answered last time, and that difference showed: a host on the
        // same network as this machine was reported offline by every sweep,
        // while Connect reached it directly and immediately. A list that is
        // confidently wrong about the one thing it exists to answer is worse
        // than no list.
        for known in known_addrs {
            addr.addrs.insert(iroh::TransportAddr::Ip(*known));
        }

        let conn = self
            .endpoint
            .connect(addr, ALPN)
            .await
            .with_context(|| {
                // Named in the same vocabulary the Connect button uses, so a
                // card and a session window explain a failure the same way.
                let step = if known_addrs.is_empty() {
                    DialStep::ViaRelay
                } else {
                    DialStep::Direct
                };
                step.stalled_at().to_string()
            })?;

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

    fn test_config() -> SessionConfig {
        SessionConfig {
            capture: CaptureConfig {
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
            "sized from the plan the window admits {from_plan:.0} fps, from the achieved              rate only {from_achieved:.0} — the second is the drag this avoids",
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
        assert_eq!(ApprovalDecision::Allow(ControlGrants::NONE).grants(), ControlGrants::NONE);
        assert_eq!(ApprovalDecision::Deny.grants(), ControlGrants::NONE);
        assert_ne!(ApprovalDecision::Allow(ControlGrants::NONE), ApprovalDecision::Deny);
        assert_eq!(ApprovalDecision::Allow(ControlGrants::ALL).grants(), ControlGrants::ALL);
    }

}
