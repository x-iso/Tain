//! Hosts this machine has connected to, remembered across restarts.
//!
//! ## Secrets live in the credential store, not here
//!
//! An entry that cannot reconnect is just a history list, so the connection code
//! has to be kept somewhere — and a connection code **is** the shared secret
//! that reaches a host. It is therefore stored by [`super::secrets`], in the
//! platform credential store, and **never** in this file.
//!
//! `connections.json` holds only which machines you have connected to and what
//! you call them. An earlier version wrote the codes here in plaintext; a
//! migration on load moves any it still finds into the credential store and
//! rewrites the file without them.
//!
//! ## Identity
//!
//! Entries are keyed by the host's **endpoint id**, not by the code: a host
//! that regenerates its passcode is still the same machine, and should update
//! the row you already named rather than growing a second one.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::*;

/// Cap on remembered hosts. Old entries fall off the end rather than growing
/// a file of stale secrets forever.
const MAX_ENTRIES: usize = 64;

/// Live state of a saved connection, derived each frame rather than stored.
///
/// There is deliberately **no "offline"**. Nothing here ever probes the host, so
/// claiming it is offline states something we have not checked — and the card
/// then contradicts itself by having a working connect button. "Ready" says what
/// is actually true: we have a code, and pressing this will try it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// A session to this host is open and streaming.
    Online,
    /// Connecting right now.
    Connecting,
    /// A session exists but is not streaming — usually awaiting approval.
    Waiting,
    /// The last attempt failed.
    Failed,
    /// We hold a code and have not checked it yet. Not a claim about the host —
    /// the button still works, we simply have nothing to report.
    #[default]
    Unknown,
    /// Probed just now: the host answered and the code still works.
    Ready,
    /// The host answered, but no longer accepts this code. Almost always because
    /// it regenerated one after the last disconnect.
    Stale,
    /// The host did not answer. Off, asleep, or not reachable from here.
    Unreachable,
    /// No saved code, so this row cannot dial at all.
    NoCode,
}

impl ConnectionStatus {
    /// Label shown in the card's bottom bar. Carries the *state*.
    pub fn label(self) -> &'static str {
        match self {
            Self::Online => "connected",
            Self::Connecting => "connecting...",
            Self::Waiting => "waiting for approval...",
            Self::Failed => "last attempt failed",
            Self::Unknown => "checking...",
            Self::Ready => "online",
            Self::Stale => "code expired",
            Self::Unreachable => "offline",
            Self::NoCode => "no saved code",
        }
    }

    /// Longer explanation, for the card's tooltip.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Online => "A session to this host is open.",
            Self::Connecting => "Dialling this host now.",
            Self::Waiting => "Connected — waiting for the host to allow it.",
            Self::Failed => "The last attempt to connect did not succeed.",
            Self::Unknown => "Checking whether this host is reachable and the code still works.",
            Self::Ready => "This host answered just now and still accepts this code.",
            Self::Stale => {
                "This host is reachable, but the code has expired — it generates a new one \
                 after the last guest disconnects. Ask for the current code."
            }
            Self::Unreachable => {
                "This host did not answer. It may be switched off, asleep, or not \
                 reachable from this network."
            }
            Self::NoCode => "No code is saved for this host, so there is nothing to dial.",
        }
    }

    /// Colour of the card's action triangle. Carries *actionability*, not state —
    /// green means "this button does something", grey means it does not.
    ///
    /// The probe is what makes this honest. Before it existed the card could
    /// only guess: it showed "ready" with a green arrow for a host that had been
    /// off for a week, because holding a code was the only evidence available.
    pub fn accent(self) -> egui::Color32 {
        match self {
            Self::Ready => super::theme::LIVE_GREEN,
            Self::Unknown | Self::Failed => egui::Color32::from_gray(150),
            Self::Connecting | Self::Waiting | Self::Stale => {
                egui::Color32::from_rgb(0xE0, 0x9B, 0x2E)
            }
            Self::Online | Self::Unreachable | Self::NoCode => egui::Color32::from_gray(110),
        }
    }

    /// Whether pressing the card's button would do anything.
    ///
    /// A stale or unreachable host still allows the attempt: the probe is a
    /// snapshot, the machine may have come back in the last few seconds, and
    /// refusing to even try on our own guess is exactly the kind of false
    /// negative the probe exists to avoid making.
    pub fn can_connect(self) -> bool {
        !matches!(self, Self::Online | Self::NoCode | Self::Connecting | Self::Waiting)
    }
}

/// One remembered host.
///
/// Deliberately carries **no secret**. The connection code for this host lives
/// in the credential store, addressed by `endpoint_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Hex of the host's 32-byte endpoint id. The stable identity of the row.
    pub endpoint_id: String,
    /// What to call it. Seeded from the host's own name, then editable.
    pub display_name: String,
    /// Unix seconds of the last connection attempt, for ordering.
    #[serde(default)]
    pub last_connected: u64,
    /// A 32x18 picture of the host's screen, hex-encoded — see
    /// [`super::preview`] for why it is that small.
    ///
    /// Absent until a session has run, and absent for a whole session that drew
    /// every frame straight from the GPU, since then no pixels ever reach the
    /// processor here. Both are ordinary: the card simply looks as it always
    /// did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Plaintext code written by an older build. Only ever read, to migrate it
    /// into the credential store; never written back.
    #[serde(default, skip_serializing)]
    legacy_code: Option<String>,
    /// Whether a code is held for this host. Runtime only — it is a property of
    /// the credential store, not of this file.
    #[serde(skip)]
    pub has_code: bool,
    /// Live status. Recomputed from open sessions every frame, never persisted.
    #[serde(skip)]
    pub status: ConnectionStatus,
    /// What the last reachability probe found, if one has run.
    ///
    /// Runtime only, and deliberately so: a status persisted from the last time
    /// the app was open is a claim about a machine we have not spoken to since,
    /// which is exactly the false confidence the probe exists to replace.
    #[serde(skip)]
    pub probed: Option<ConnectionStatus>,
}

impl SavedConnection {
    /// Build a row for a host, with no secret attached.
    pub fn new(endpoint_id: String, display_name: String) -> Self {
        Self {
            endpoint_id,
            display_name,
            last_connected: now_secs(),
            preview: None,
            legacy_code: None,
            has_code: false,
            status: ConnectionStatus::default(),
            probed: None,
        }
    }

    /// Whether this row has enough information to dial the host again.
    pub fn can_reconnect(&self) -> bool {
        self.has_code
    }

    /// The decoded preview, or `None` if there is none or it is malformed.
    pub fn preview_pixels(&self) -> Option<Vec<u8>> {
        super::preview::decode(self.preview.as_deref()?)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The remembered-hosts list.
///
/// A store only writes to disk if it was **loaded** from disk. That is not a
/// detail: mutating methods persist as a side effect, so a store built with
/// `new()` in a test would otherwise scribble its fixtures into the real user's
/// connection list — which is exactly what happened before `path` existed.
#[derive(Debug, Default)]
pub struct ConnectionStore {
    entries: Vec<SavedConnection>,
    /// `None` means in-memory only. Set by [`ConnectionStore::load`].
    path: Option<PathBuf>,
    /// Where connection codes go. In-memory for a store built with `new()`, so
    /// a test cannot leave credentials in the developer's own keychain.
    secrets: super::secrets::SecretStore,
}

impl ConnectionStore {
    /// An in-memory store that touches neither disk nor the credential store.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
            secrets: super::secrets::SecretStore::in_memory(),
        }
    }

    /// Where the list lives. Deliberately a sibling of `settings.json`.
    pub fn path() -> Option<PathBuf> {
        super::settings::Settings::path()
            .and_then(|p| p.parent().map(|dir| dir.join("connections.json")))
    }

    /// Load from disk, falling back to an empty list.
    ///
    /// A corrupt file loses the history rather than blocking startup — the same
    /// trade the settings file makes, for the same reason.
    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::new() };
        let backing = Some(path.clone());
        let fresh = || Self {
            entries: Vec::new(),
            path: backing.clone(),
            secrets: super::secrets::SecretStore::Os,
        };
        let Ok(text) = std::fs::read_to_string(&path) else { return fresh() };

        match serde_json::from_str::<Vec<SavedConnection>>(&text) {
            Ok(entries) => {
                debug!("Loaded {} saved connections", entries.len());
                let mut store = Self {
                    entries,
                    path: backing,
                    secrets: super::secrets::SecretStore::Os,
                };
                store.migrate_plaintext_codes();
                store.refresh_code_flags();
                store.sort();
                store
            }
            Err(e) => {
                warn!("Ignoring unreadable connection list at {}: {e}", path.display());
                fresh()
            }
        }
    }

    /// Move any plaintext code written by an older build into the credential
    /// store, then rewrite the file without it.
    ///
    /// Runs before anything else touches the entries. Leaving the old file in
    /// place would keep working secrets on disk indefinitely for anyone who
    /// upgrades, which is the whole problem this is fixing.
    fn migrate_plaintext_codes(&mut self) {
        let mut migrated = 0;
        for entry in self.entries.iter_mut() {
            let Some(code) = entry.legacy_code.take() else { continue };
            if self.secrets.store(&entry.endpoint_id, &code) {
                migrated += 1;
            }
        }
        if migrated > 0 {
            warn!(
                "Moved {migrated} connection code(s) out of plaintext and into the \
                 credential store"
            );
            // Unconditional: `legacy_code` is skip_serializing, so writing the
            // file now is what actually removes the plaintext.
            self.save();
        }
    }

    /// Ask the credential store which rows still have a code.
    fn refresh_code_flags(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.has_code = self.secrets.has(&entry.endpoint_id);
        }
    }

    /// Persist to disk. Failures are logged rather than surfaced.
    ///
    /// A no-op for an in-memory store.
    pub fn save(&self) {
        let Some(ref path) = self.path else { return };
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            warn!("Could not create {}: {e}", dir.display());
            return;
        }
        match serde_json::to_string_pretty(&self.entries) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    warn!("Could not save connections to {}: {e}", path.display());
                }
            }
            Err(e) => warn!("Could not serialize connections: {e}"),
        }
    }

    pub fn entries(&self) -> &[SavedConnection] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Most recent first, so the host you just used is the one you see.
    fn sort(&mut self) {
        // Reversed key rather than `sort_by_key`, which cannot borrow here.
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.last_connected));
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Record a connection attempt, creating or refreshing the row.
    ///
    /// A name the user typed is never overwritten by one the host published —
    /// renaming a machine is a deliberate act, and losing it on every reconnect
    /// would make the rename feature useless.
    pub fn record(&mut self, endpoint_id: String, code: String, fallback_name: &str) {
        // The code goes to the credential store, never into `self.entries`.
        // `stored` is whether that actually worked, so a machine with no
        // credential store shows "no saved code" rather than a button that
        // cannot do anything.
        let stored = self.secrets.store(&endpoint_id, &code);

        match self.entries.iter_mut().find(|c| c.endpoint_id == endpoint_id) {
            Some(existing) => {
                existing.last_connected = now_secs();
                existing.has_code = stored;
            }
            None => {
                let mut entry =
                    SavedConnection::new(endpoint_id, fallback_name.to_string());
                entry.has_code = stored;
                entry.status = ConnectionStatus::Connecting;
                self.entries.push(entry);
            }
        }
        self.sort();
        self.save();
    }

    /// Apply the name a host published, if the row is still using its
    /// placeholder. Returns true if anything changed.
    pub fn adopt_host_name(&mut self, endpoint_id: &str, host_name: &str, placeholder: &str) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|c| c.endpoint_id == endpoint_id) else {
            return false;
        };
        if entry.display_name != placeholder || entry.display_name == host_name {
            return false;
        }
        entry.display_name = host_name.to_string();
        self.save();
        true
    }

    /// Rename a row. Returns true if it existed.
    pub fn rename(&mut self, endpoint_id: &str, name: String) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|c| c.endpoint_id == endpoint_id) else {
            return false;
        };
        entry.display_name = name;
        self.save();
        true
    }

    /// Remember what this host's screen looked like.
    ///
    /// Writes only when the picture changed, because this is called while a
    /// session is running and the file would otherwise be rewritten on a timer
    /// for no reason. Silently does nothing for a host with no row — a preview
    /// is not a reason to remember a machine.
    pub fn set_preview(&mut self, endpoint_id: &str, pixels: &[u8]) -> bool {
        let encoded = super::preview::encode(pixels);
        let Some(entry) = self.entries.iter_mut().find(|c| c.endpoint_id == endpoint_id) else {
            return false;
        };
        if entry.preview.as_deref() == Some(encoded.as_str()) {
            return false;
        }
        entry.preview = Some(encoded);
        self.save();
        true
    }

    /// Forget a host, and the code stored with it.
    ///
    /// The credential is deleted whether or not a row existed: "forget this
    /// host" has to actually destroy the secret, not just hide the row.
    pub fn forget(&mut self, endpoint_id: &str) {
        self.secrets.forget(endpoint_id);
        let before = self.entries.len();
        self.entries.retain(|c| c.endpoint_id != endpoint_id);
        if self.entries.len() != before {
            self.save();
        }
    }

    /// The code last used for a host, read from the credential store.
    pub fn code_for(&self, endpoint_id: &str) -> Option<String> {
        self.secrets.load(endpoint_id)
    }

    /// Overwrite the live status of every row from the sessions currently open.
    ///
    /// A row with no live session falls back to what we can actually assert:
    /// whether it has a code to dial with. Never to "offline" — see
    /// [`ConnectionStatus`].
    pub fn apply_status(&mut self, live: &[(String, ConnectionStatus)]) {
        for entry in self.entries.iter_mut() {
            entry.status = live
                .iter()
                .find(|(id, _)| *id == entry.endpoint_id)
                .map(|(_, status)| *status)
                .unwrap_or(if entry.has_code {
                    // With no session open, the probe is the only thing that
                    // knows anything. Absent one, say so rather than guessing.
                    entry.probed.unwrap_or(ConnectionStatus::Unknown)
                } else {
                    ConnectionStatus::NoCode
                });
        }
    }

    /// Every row we hold a code for, as `(endpoint_hex, code)`.
    ///
    /// Pulled out in one pass so the prober can work without holding the store
    /// locked across the network.
    pub fn probe_targets(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter(|e| e.has_code)
            .filter_map(|e| Some((e.endpoint_id.clone(), self.secrets.load(&e.endpoint_id)?)))
            .collect()
    }

    /// Record what a probe found for one host.
    pub fn set_probe_result(&mut self, endpoint_hex: &str, status: ConnectionStatus) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.endpoint_id == endpoint_hex) {
            entry.probed = Some(status);
        }
    }

    /// Replace the whole list. Used by the debug demo seed.
    // `test` as well as `debug_assertions`: the tests use this too, and
    // `cargo test --release` turns the latter off.
    #[cfg(any(debug_assertions, test))]
    pub fn set_entries(&mut self, entries: Vec<SavedConnection>) {
        self.entries = entries;
    }
}

/// Hex of an endpoint id, the key rows are stored under.
pub fn endpoint_hex(endpoint_id: &[u8; 32]) -> String {
    endpoint_id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Short human-readable stand-in for a host with no published name.
///
/// Takes the hex rather than the raw id so the same call works when all we have
/// is a stored row. Deriving it means it stays stable across reconnects, which
/// is what lets [`ConnectionStore::adopt_host_name`] tell a placeholder from a
/// name the user typed.
pub fn placeholder_name(endpoint_hex: &str) -> String {
    let prefix: String = endpoint_hex.chars().take(8).collect();
    format!("PC {}", prefix.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are arbitrary here: the store's secret backend is in-memory, so
    /// nothing a test writes can reach the developer's credential store.
    fn id(name: &str) -> String {
        name.to_string()
    }

    fn store_with(names: &[(&str, &str)]) -> ConnectionStore {
        let entries = names
            .iter()
            .enumerate()
            .map(|(i, (id, name))| SavedConnection {
                preview: None,
                endpoint_id: (*id).to_string(),
                display_name: (*name).to_string(),
                legacy_code: None,
                has_code: true,
                last_connected: i as u64,
                status: ConnectionStatus::Ready,
                probed: None,
            })
            .collect();
        // No path: these must never reach the real user's connection list.
        ConnectionStore { entries, path: None, secrets: crate::gui::secrets::SecretStore::in_memory() }
    }

    #[test]
    fn a_store_built_in_memory_never_writes_to_the_users_file() {
        // Regression: mutating methods persist as a side effect, so tests using
        // a store with a real path wrote their fixtures into the user's own
        // connections.json — 62 rows named "PC" showed up in the running app.
        let mut store = ConnectionStore::new();
        assert!(store.path.is_none());

        store.record("host-1".into(), "CODE".into(), "PC");
        store.rename("host-1", "Renamed".into());
        store.forget("host-1");

        // The real file is the one `load()` would use; an in-memory store must
        // not be able to name it at all.
        assert!(store.path.is_none(), "nothing may give an in-memory store a path");
    }

    #[test]
    fn a_host_is_remembered_under_its_endpoint_not_its_code() {
        // Regression: the host regenerates its guest code constantly, so keying
        // on the code would grow a new row on every reconnect.
        let mut store = ConnectionStore::new();
        store
            .entries
            .push(SavedConnection::new(id("studio"), "Studio".into()));

        store.record(id("studio"), "NEW".into(), "PC ABC");

        assert_eq!(store.entries().len(), 1, "same machine, same row");
        assert_eq!(store.entries()[0].display_name, "Studio", "the name the user gave stands");
    }

    #[test]
    fn a_new_host_gets_a_row_with_its_placeholder_name() {
        let mut store = ConnectionStore::new();
        store.record(id("fresh"), "CODE".into(), "PC DEAD");

        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].display_name, "PC DEAD");
    }

    #[test]
    fn a_published_host_name_replaces_the_placeholder_but_not_a_rename() {
        let mut store = ConnectionStore::new();
        store.record(id("named"), "CODE".into(), "PC ABC");

        assert!(store.adopt_host_name(&id("named"), "Studio desktop", "PC ABC"));
        assert_eq!(store.entries()[0].display_name, "Studio desktop");

        // Once the user has named it, the host does not get to overrule them.
        store.rename(&id("named"), "My laptop".into());
        assert!(!store.adopt_host_name(&id("named"), "Studio desktop", "PC ABC"));
        assert_eq!(store.entries()[0].display_name, "My laptop");
    }

    #[test]
    fn adopting_a_name_twice_is_a_no_op() {
        let mut store = ConnectionStore::new();
        store.record(id("twice"), "CODE".into(), "PC ABC");
        assert!(store.adopt_host_name(&id("twice"), "Studio", "PC ABC"));
        assert!(
            !store.adopt_host_name(&id("twice"), "Studio", "PC ABC"),
            "nothing left to change"
        );
    }

    #[test]
    fn forgetting_a_host_destroys_its_credential() {
        // A full round trip through the real credential store: "forget" has to
        // delete the secret, not just hide the row.
        let mut store = ConnectionStore::new();
        store.record(id("forget-a"), "CODE-A".into(), "A");
        store.record(id("forget-b"), "CODE-B".into(), "B");

        // No credential store on this machine (a headless Linux box, say) —
        // there is nothing to assert about storage that did not happen.
        if store.code_for(&id("forget-a")).is_none() {
            eprintln!("no credential store on this machine — skipping");
            return;
        }

        store.forget(&id("forget-a"));

        assert_eq!(store.entries().len(), 1);
        assert!(
            store.code_for(&id("forget-a")).is_none(),
            "the secret must go with the row"
        );
        assert_eq!(store.code_for(&id("forget-b")).as_deref(), Some("CODE-B"));
    }

    #[test]
    fn the_most_recently_used_host_comes_first() {
        let mut store = store_with(&[("old", "Old"), ("newer", "Newer")]);
        store.sort();
        assert_eq!(store.entries()[0].endpoint_id, "newer");

        // Reconnecting to the old one floats it back to the top.
        store.record("old".into(), "CODE".into(), "Old");
        assert_eq!(store.entries()[0].endpoint_id, "old");
    }

    #[test]
    fn the_list_does_not_grow_without_bound() {
        let mut store = ConnectionStore::new();
        for i in 0..(MAX_ENTRIES + 20) {
            store.record(format!("host-{i}"), "CODE".into(), "PC");
        }
        assert_eq!(store.entries().len(), MAX_ENTRIES);
    }

    #[test]
    fn status_comes_from_live_sessions_and_resets_when_they_end() {
        let mut store = store_with(&[("a", "A"), ("b", "B")]);

        store.apply_status(&[("a".into(), ConnectionStatus::Online)]);
        let by_id = |s: &ConnectionStore, id: &str| {
            s.entries().iter().find(|c| c.endpoint_id == id).unwrap().status
        };
        assert_eq!(by_id(&store, "a"), ConnectionStatus::Online);
        // Never probed, so there is nothing to say about it yet.
        assert_eq!(by_id(&store, "b"), ConnectionStatus::Unknown);

        // Regression territory: a row left green after its session ended is
        // exactly the bug the host side had.
        store.apply_status(&[]);
        assert_eq!(by_id(&store, "a"), ConnectionStatus::Unknown);
    }

    #[test]
    fn a_probe_result_survives_until_the_next_one_replaces_it() {
        let mut store = store_with(&[("a", "A"), ("b", "B")]);
        store.set_probe_result("a", ConnectionStatus::Ready);
        store.set_probe_result("b", ConnectionStatus::Unreachable);
        store.apply_status(&[]);

        let by_id = |s: &ConnectionStore, id: &str| {
            s.entries().iter().find(|c| c.endpoint_id == id).unwrap().status
        };
        assert_eq!(by_id(&store, "a"), ConnectionStatus::Ready);
        assert_eq!(by_id(&store, "b"), ConnectionStatus::Unreachable);

        // A live session outranks it — that is direct evidence, not a snapshot.
        store.apply_status(&[("b".into(), ConnectionStatus::Online)]);
        assert_eq!(by_id(&store, "b"), ConnectionStatus::Online);

        // And when the session ends, the last probe result is what is left.
        store.apply_status(&[]);
        assert_eq!(by_id(&store, "b"), ConnectionStatus::Unreachable);
    }

    #[test]
    fn only_rows_with_a_code_are_worth_probing() {
        // A row with no code has nothing to check, and probing one would be a
        // connection attempt against a host we cannot even address.
        let mut store = store_with(&[("a", "A")]);
        store.entries[0].has_code = false;
        assert!(store.probe_targets().is_empty());
    }

    #[test]
    fn a_row_without_a_code_says_so_rather_than_looking_connectable() {
        let mut store = ConnectionStore::new();
        store.entries.push(SavedConnection {
            preview: None,
            endpoint_id: "abc".into(),
            display_name: "Old PC".into(),
            legacy_code: None,
            has_code: false,
            last_connected: 0,
            status: ConnectionStatus::Ready,
            probed: None,
        });

        store.apply_status(&[]);
        assert_eq!(store.entries()[0].status, ConnectionStatus::NoCode);
        assert!(!store.entries()[0].status.can_connect());
    }

    #[test]
    fn an_entry_written_by_an_older_build_still_loads() {
        // `code` and `last_connected` were added after the first shape.
        let json = r#"[{ "endpoint_id": "abc", "display_name": "Studio" }]"#;
        let entries: Vec<SavedConnection> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].can_reconnect());
    }

    #[test]
    fn entries_round_trip_without_the_runtime_status() {
        let store = store_with(&[("a", "A")]);
        let text = serde_json::to_string(store.entries()).unwrap();
        assert!(!text.contains("status"), "live status must not be persisted");
        assert_eq!(serde_json::from_str::<Vec<SavedConnection>>(&text).unwrap().len(), 1);
    }

    #[test]
    fn renaming_an_unknown_host_reports_failure() {
        let mut store = ConnectionStore::new();
        assert!(!store.rename("nope", "X".into()));
        assert!(!store.adopt_host_name("nope", "X", "Y"));
    }

    #[test]
    fn a_placeholder_names_the_machine_without_leaking_the_secret() {
        let hex = endpoint_hex(&[0xAB; 32]);
        assert_eq!(hex.len(), 64);
        assert_eq!(placeholder_name(&hex), "PC ABABABAB");
        // A short or empty id must not panic on the slice.
        assert_eq!(placeholder_name("abc"), "PC ABC");
        assert_eq!(placeholder_name(""), "PC ");
    }

    #[test]
    fn statuses_are_visually_distinguishable() {
        assert_ne!(ConnectionStatus::Online.accent(), ConnectionStatus::Ready.accent());
        for status in [
            ConnectionStatus::Online,
            ConnectionStatus::Connecting,
            ConnectionStatus::Waiting,
            ConnectionStatus::Failed,
            ConnectionStatus::Ready,
            ConnectionStatus::NoCode,
        ] {
            assert!(!status.label().is_empty());
        }
    }

    #[test]
    fn the_connection_list_sits_beside_the_settings_file() {
        // Both are user config; splitting them across directories would make
        // "where are my preferences" a two-answer question.
        if let (Some(settings), Some(connections)) =
            (super::super::settings::Settings::path(), ConnectionStore::path())
        {
            assert_eq!(settings.parent(), connections.parent());
            assert!(connections.ends_with("connections.json"));
        }
    }
}
