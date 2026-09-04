//! Session management for tracking active guest connections.
//!
//! The host can have multiple concurrent guest sessions — each with its own
//! session ID, input grants, and outbound message channel.
//!
//! Session IDs are assigned **by the host**, never taken from the guest's `Auth`
//! message: a guest that picked its own ID could collide with (or impersonate)
//! another session's approval state.
//!
//! Input grants are tracked *per session*, not per passcode secret. A single
//! `GUEST-` passcode is shared by everyone the host hands it to, so keying
//! approval on the secret would mean approving one guest approves all of them.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::*;

use super::protocol::{ControlGrants, ControlStateMessage, Message};

/// Tracks all active sessions on the host side.
///
/// Uses an internal `std::sync::Mutex` — every method takes `&self` and releases
/// the lock before returning, so callers never hold a guard across an `.await`.
pub struct SessionManager {
    inner: Mutex<SessionState>,
}

struct SessionState {
    sessions: HashMap<u32, SessionInfo>,
    /// Per-session channel into the writer loop, for pushing host-initiated messages.
    outbound: HashMap<u32, mpsc::UnboundedSender<Message>>,
    next_session_id: u32,
}

/// Which session has taken which virtual display, for its own size.
///
/// Process-wide rather than per `SessionManager`, because the thing being
/// arbitrated is process-wide: there is one desktop, and a display can only be
/// one size at a time.
static CLAIMED_DISPLAYS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

/// Claim a virtual display for a session, or say who already has it.
///
/// Returns `None` when the claim succeeded — including when this session
/// already held it, so a guest resizing its window keeps following.
///
/// # Why first come rather than most recent
///
/// A display has one size. Two sessions both resizing the same one to their own
/// window would take it from each other for as long as both were connected,
/// and the picture would move under both of them. The first to ask keeps it;
/// the second is told no and streams scaled, which is what every session did
/// before any of this existed.
pub fn claim_virtual_display(name: &str, session_id: u32) -> Option<u32> {
    let mut guard = CLAIMED_DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    let claims = guard.get_or_insert_with(HashMap::new);
    match claims.get(name) {
        Some(&holder) if holder != session_id => Some(holder),
        _ => {
            claims.insert(name.to_string(), session_id);
            None
        }
    }
}

/// Let go of everything a session claimed, when it ends.
///
/// Without this a display stays spoken for until the process exits, and the
/// next guest to watch it would be told it belongs to a session that is gone.
pub fn release_virtual_displays(session_id: u32) {
    let mut guard = CLAIMED_DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(claims) = guard.as_mut() {
        claims.retain(|_, holder| *holder != session_id);
    }
}

/// Information about a single guest connection.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The passcode secret used for authentication.
    pub secret: [u8; 32],
    /// Which input devices this session may drive.
    pub grants: ControlGrants,
    /// Display configuration sent by the guest.
    pub display_config: Option<super::protocol::DisplayConfig>,
    /// Passcode type — distinguishes Master vs Guest sessions for auto-regen logic.
    pub passcode_type: super::passcode::PasscodeType,
    /// Human-friendly label for this connection, for logs and the UI.
    pub label: String,
    /// Latest report from the capture pipeline: what the stream is running at,
    /// and which stage is holding it back. `None` until the first evaluation
    /// window closes.
    pub stream_status: Option<super::protocol::StreamStatus>,
    /// The address the guest actually reached this host on.
    ///
    /// Only ever a direct path — a relayed connection is refused rather than
    /// streamed — so this is the answer to "how is this guest connected", which
    /// the host had in a log line and nowhere a person would look.
    pub route: Option<String>,
    /// The guest's own view of the link, as it last reported it.
    ///
    /// The host cannot measure this half: round trip and decode rate are things
    /// only the far end knows. Kept beside `stream_status`, which is this end's
    /// half of the same picture.
    pub link: Option<super::protocol::GuestTelemetry>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SessionState {
                sessions: HashMap::new(),
                outbound: HashMap::new(),
                next_session_id: 1,
            }),
        }
    }

    /// Register a new session and return the host-assigned session ID.
    pub fn add_session(
        &self,
        secret: [u8; 32],
        grants: ControlGrants,
        passcode_type: super::passcode::PasscodeType,
    ) -> u32 {
        let mut state = self.inner.lock().unwrap();
        let id = state.next_session_id;
        state.next_session_id += 1;

        state.sessions.insert(id, SessionInfo {
            secret,
            grants,
            display_config: None,
            passcode_type,
            label: format!("Guest #{id}"),
            stream_status: None,
            route: None,
            link: None,
        });

        info!("Session #{} added (mouse={}, keyboard={})", id, grants.mouse, grants.keyboard);
        id
    }

    /// Give a session a human-friendly label. Returns false if unknown.
    pub fn set_label(&self, session_id: u32, label: String) -> bool {
        let mut state = self.inner.lock().unwrap();
        match state.sessions.get_mut(&session_id) {
            Some(info) => {
                info.label = label;
                true
            }
            None => false,
        }
    }

    /// Label for a session, if it is active.
    /// Which passcode a session authenticated with.
    ///
    /// The host's UI needs this to tell an administrator's own connection apart
    /// from a guest's: the first grants control outright and never asks, so it
    /// would otherwise appear in no list at all and could not be ended.
    pub fn passcode_type(&self, session_id: u32) -> Option<super::passcode::PasscodeType> {
        let state = self.inner.lock().unwrap();
        state.sessions.get(&session_id).map(|s| s.passcode_type)
    }

    pub fn label(&self, session_id: u32) -> Option<String> {
        let state = self.inner.lock().unwrap();
        state.sessions.get(&session_id).map(|s| s.label.clone())
    }

    /// Attach the writer channel so grant changes can be pushed to the guest.
    pub fn register_outbound(&self, session_id: u32, tx: mpsc::UnboundedSender<Message>) {
        let mut state = self.inner.lock().unwrap();
        state.outbound.insert(session_id, tx);
    }

    /// Remove a session by ID. Must be called when a connection ends, or
    /// `guest_session_count` never drops and passcode auto-regen never fires.
    pub fn remove_session(&self, session_id: u32) -> Option<SessionInfo> {
        let mut state = self.inner.lock().unwrap();
        state.outbound.remove(&session_id);
        let info = state.sessions.remove(&session_id);
        if info.is_some() {
            info!("Session #{} removed", session_id);
        }
        // Whatever it had taken for its own size goes back, or the next guest
        // to watch that display is refused on behalf of a session that is gone.
        release_virtual_displays(session_id);
        info
    }

    /// Set which devices a session may drive. Returns false if unknown.
    ///
    /// Takes effect immediately on a live session — the input path re-reads grants
    /// on every event — and notifies the guest so its UI can reflect reality.
    pub fn set_control(&self, session_id: u32, grants: ControlGrants) -> bool {
        let notify = {
            let mut state = self.inner.lock().unwrap();
            let Some(info) = state.sessions.get_mut(&session_id) else {
                warn!("set_control for unknown session #{}", session_id);
                return false;
            };
            info.grants = grants;
            info!(
                "Session #{} grants: mouse={}, keyboard={}",
                session_id, grants.mouse, grants.keyboard
            );
            state.outbound.get(&session_id).cloned()
        };

        // Send after releasing the lock — the writer loop may be draining this
        // channel on another thread.
        if let Some(tx) = notify {
            let _ = tx.send(Message::ControlState(ControlStateMessage { session_id, grants }));
        }
        true
    }

    /// Disconnect a session from the host side.
    ///
    /// Pushes a `SessionEnd` down the session's outbound channel; the writer
    /// loop closes the stream once it has been sent, so the guest is told *why*
    /// it was dropped rather than just seeing the connection vanish.
    /// Returns false if the session is not (or no longer) active.
    pub fn kick(&self, session_id: u32, reason: &str) -> bool {
        let outbound = {
            let state = self.inner.lock().unwrap();
            if !state.sessions.contains_key(&session_id) {
                return false;
            }
            state.outbound.get(&session_id).cloned()
        };

        info!("Session #{} kicked by the host ({reason})", session_id);
        match outbound {
            Some(tx) => tx
                .send(Message::SessionEnd(super::protocol::SessionEnd {
                    session_id,
                    reason: reason.to_string(),
                }))
                .is_ok(),
            // No writer attached yet; removing the session is the best we can do.
            None => {
                self.remove_session(session_id);
                true
            }
        }
    }

    /// Current grants for a session, if it is active.
    pub fn grants(&self, session_id: u32) -> Option<ControlGrants> {
        let state = self.inner.lock().unwrap();
        state.sessions.get(&session_id).map(|s| s.grants)
    }

    /// Whether the session may drive at least one input device.
    pub fn has_any_control(&self, session_id: u32) -> bool {
        self.grants(session_id).is_some_and(|g| g.any())
    }

    /// Get all active session IDs.
    pub fn active_sessions(&self) -> Vec<u32> {
        let state = self.inner.lock().unwrap();
        state.sessions.keys().copied().collect()
    }

    /// Snapshot of a single session.
    pub fn get(&self, session_id: u32) -> Option<SessionInfo> {
        let state = self.inner.lock().unwrap();
        state.sessions.get(&session_id).cloned()
    }

    /// Update the display config for a session.
    pub fn set_display_config(&self, session_id: u32, cfg: super::protocol::DisplayConfig) -> bool {
        let mut state = self.inner.lock().unwrap();
        if let Some(info) = state.sessions.get_mut(&session_id) {
            info.display_config = Some(cfg);
            true
        } else {
            false
        }
    }

    /// Record what the stream is running at and what is limiting it.
    ///
    /// Written by the capture pipeline, read by the host's UI — the host should
    /// be able to see that a guest's picture is struggling, and why, without
    /// waiting to be told.
    pub fn set_stream_status(&self, session_id: u32, status: super::protocol::StreamStatus) -> bool {
        let mut state = self.inner.lock().unwrap();
        if let Some(info) = state.sessions.get_mut(&session_id) {
            info.stream_status = Some(status);
            true
        } else {
            false
        }
    }

    /// Record the address a guest reached this host on.
    pub fn set_route(&self, session_id: u32, route: String) -> bool {
        let mut state = self.inner.lock().unwrap();
        if let Some(info) = state.sessions.get_mut(&session_id) {
            info.route = Some(route);
            true
        } else {
            false
        }
    }

    /// Record the guest's latest report on the link.
    pub fn set_link(&self, session_id: u32, link: super::protocol::GuestTelemetry) -> bool {
        let mut state = self.inner.lock().unwrap();
        if let Some(info) = state.sessions.get_mut(&session_id) {
            info.link = Some(link);
            true
        } else {
            false
        }
    }

    /// Everything known about one session, for a UI that wants to show it.
    ///
    /// One call rather than an accessor per field: the host's connection card
    /// reads five of these at once, and taking the lock five times to build one
    /// tooltip is five chances to disagree with itself.
    pub fn info(&self, session_id: u32) -> Option<SessionInfo> {
        let state = self.inner.lock().unwrap();
        state.sessions.get(&session_id).cloned()
    }

    /// Get the number of active sessions.
    pub fn count(&self) -> usize {
        let state = self.inner.lock().unwrap();
        state.sessions.len()
    }

    /// Number of active GUEST-type sessions (Master sessions don't trigger regen).
    pub fn guest_session_count(&self) -> usize {
        let state = self.inner.lock().unwrap();
        state
            .sessions
            .values()
            .filter(|s| s.passcode_type == super::passcode::PasscodeType::Guest)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passcode::PasscodeType;
    use crate::protocol::InputDevice;

    #[test]
    fn test_session_lifecycle() {
        let mgr = SessionManager::new();
        let secret = [42u8; 32];

        let id = mgr.add_session(secret, ControlGrants::NONE, PasscodeType::Guest);
        assert_eq!(mgr.count(), 1);
        assert!(!mgr.has_any_control(id));

        assert!(mgr.set_control(id, ControlGrants::ALL));
        assert!(mgr.has_any_control(id));

        let info = mgr.remove_session(id).unwrap();
        assert_eq!(info.secret, secret);
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn mouse_and_keyboard_are_granted_independently() {
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);

        mgr.set_control(id, ControlGrants::new(true, false));
        let g = mgr.grants(id).unwrap();
        assert!(g.allows(InputDevice::Mouse));
        assert!(!g.allows(InputDevice::Keyboard));

        mgr.set_control(id, ControlGrants::new(false, true));
        let g = mgr.grants(id).unwrap();
        assert!(!g.allows(InputDevice::Mouse));
        assert!(g.allows(InputDevice::Keyboard));
    }

    #[tokio::test]
    async fn granting_control_notifies_the_guest() {
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);

        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.register_outbound(id, tx);

        mgr.set_control(id, ControlGrants::new(true, false));

        match rx.recv().await.expect("guest should be told about the change") {
            Message::ControlState(cs) => {
                assert_eq!(cs.session_id, id);
                assert_eq!(cs.grants, ControlGrants::new(true, false));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn session_ids_are_unique_and_host_assigned() {
        let mgr = SessionManager::new();
        let a = mgr.add_session([1u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let b = mgr.add_session([1u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        assert_ne!(a, b, "each connection must get a distinct host-assigned id");
    }

    #[test]
    fn approving_one_guest_does_not_approve_another_on_the_same_passcode() {
        // Both guests authenticate with the *same* shared GUEST passcode secret.
        let mgr = SessionManager::new();
        let shared_secret = [7u8; 32];
        let a = mgr.add_session(shared_secret, ControlGrants::NONE, PasscodeType::Guest);
        let b = mgr.add_session(shared_secret, ControlGrants::NONE, PasscodeType::Guest);

        mgr.set_control(a, ControlGrants::ALL);

        assert!(mgr.has_any_control(a));
        assert!(!mgr.has_any_control(b), "approval must not leak across sessions");
    }

    #[test]
    fn guest_session_count_tracks_only_guests() {
        let mgr = SessionManager::new();
        let m = mgr.add_session([1u8; 32], ControlGrants::ALL, PasscodeType::Master);
        let g = mgr.add_session([2u8; 32], ControlGrants::NONE, PasscodeType::Guest);

        assert_eq!(mgr.guest_session_count(), 1);
        mgr.remove_session(m);
        assert_eq!(mgr.guest_session_count(), 1);
        mgr.remove_session(g);
        assert_eq!(mgr.guest_session_count(), 0);
    }

    #[test]
    fn unknown_session_is_not_granted_control() {
        let mgr = SessionManager::new();
        assert!(!mgr.has_any_control(999));
        assert!(mgr.grants(999).is_none());
        assert!(!mgr.set_control(999, ControlGrants::ALL));
    }

    #[tokio::test]
    async fn kicking_tells_the_guest_why_before_closing() {
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::ALL, PasscodeType::Guest);
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.register_outbound(id, tx);

        assert!(mgr.kick(id, "removed by host"));

        match rx.recv().await.expect("guest should be told") {
            Message::SessionEnd(se) => {
                assert_eq!(se.session_id, id);
                assert_eq!(se.reason, "removed by host");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn kicking_an_unknown_session_reports_failure() {
        let mgr = SessionManager::new();
        assert!(!mgr.kick(999, "nope"));
    }

    #[test]
    fn a_session_has_a_generated_label_until_the_guest_names_itself() {
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        assert_eq!(mgr.label(id).unwrap(), format!("Guest #{id}"));

        assert!(mgr.set_label(id, "Studio PC".into()));
        assert_eq!(mgr.label(id).unwrap(), "Studio PC");

        assert!(!mgr.set_label(999, "nobody".into()));
        assert!(mgr.label(999).is_none());
    }

    #[test]
    fn kicking_a_session_with_no_writer_still_removes_it() {
        // The connection exists but its writer loop has not registered yet;
        // dropping the entry is the best available outcome.
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);

        assert!(mgr.kick(id, "too early"));
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn removing_a_session_drops_its_outbound_channel() {
        let mgr = SessionManager::new();
        let id = mgr.add_session([0u8; 32], ControlGrants::NONE, PasscodeType::Guest);
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.register_outbound(id, tx);

        mgr.remove_session(id);
        assert!(rx.try_recv().is_err(), "no message expected");
        // Sender was dropped with the session, so the channel is closed.
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
    }
}
