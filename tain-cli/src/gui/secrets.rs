//! Connection codes, kept in the operating system's credential store.
//!
//! A connection code **is** the shared secret that reaches a host. An earlier
//! version wrote codes into `connections.json` in plaintext, which made a file
//! full of working keys sitting in the user's config directory — readable by any
//! process running as them, and trivially picked up by a backup or a synced
//! profile folder. That is not acceptable for something people install on their
//! own machines.
//!
//! Codes now live in the platform credential store — Windows Credential Manager,
//! macOS Keychain, or the Secret Service on Linux — which encrypts them at rest
//! under the user's own login and is the mechanism every other application on
//! the machine already uses for the same job. `connections.json` keeps only
//! non-secret metadata: which machines you have connected to, and what you call
//! them.
//!
//! ## Failure is not fatal
//!
//! A missing or locked credential store (a headless Linux box with no Secret
//! Service, for instance) means the app cannot *remember* codes. It must not
//! mean the app cannot run. Every operation here degrades to "no saved code",
//! which the UI already handles — that state exists anyway for connections
//! recorded before this feature.
//!
//! Never fall back to writing the code somewhere else. Silently downgrading
//! encrypted storage to a plaintext file is exactly the bug this replaced.

use std::collections::HashMap;
use std::sync::Mutex;
use tracing::*;

/// Service name the credentials are filed under.
const SERVICE: &str = "Tain";

/// Where connection codes are kept.
///
/// Two backends, and the split is deliberate rather than incidental. A test that
/// reaches the real credential store leaves working secrets in the developer's
/// Windows Credential Manager — 87 of them, the first time this was written with
/// free functions and no seam. Namespacing the ids and cleaning up afterwards
/// was not enough: one test that forgot to opt in put them all back.
///
/// So the *type* carries the decision. [`SecretStore::in_memory`] cannot touch
/// the OS at all, which is what tests construct, and there is no path from one
/// to the other. This mirrors [`super::connections::ConnectionStore`]'s
/// `path: Option<PathBuf>` for exactly the same reason.
#[derive(Debug)]
pub enum SecretStore {
    /// The platform credential store.
    Os,
    /// A map that lives and dies with this value. Tests only.
    Memory(Mutex<HashMap<String, String>>),
}

impl Default for SecretStore {
    /// **In-memory**, not the real credential store.
    ///
    /// The unsafe direction of this choice is the one that matters. `Default` is
    /// what a caller reaches for without thinking — including
    /// [`super::connections::ConnectionStore::default`] — and a default that
    /// wrote to the developer's Windows Credential Manager is precisely the bug
    /// this type was introduced to make impossible. Production names
    /// [`SecretStore::Os`] explicitly, which is the one place that decision
    /// should ever be made.
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SecretStore {
    /// A store that never touches the operating system.
    pub fn in_memory() -> Self {
        Self::Memory(Mutex::new(HashMap::new()))
    }

    /// One stored connection code, addressed by the host's endpoint id.
    fn entry(endpoint_id: &str) -> Option<keyring::Entry> {
        match keyring::Entry::new(SERVICE, endpoint_id) {
            Ok(entry) => Some(entry),
            Err(e) => {
                // Logged rather than surfaced: the caller's job is to carry on
                // without a saved code, not to explain the platform.
                debug!("No credential store available: {e}");
                None
            }
        }
    }

    /// Store the code for a host, replacing any previous one.
    ///
    /// Returns whether it was actually saved, so the caller can reflect reality
    /// rather than claiming a connection is reconnectable when it is not.
    pub fn store(&self, endpoint_id: &str, code: &str) -> bool {
        match self {
            Self::Memory(map) => {
                map.lock().unwrap().insert(endpoint_id.to_string(), code.to_string());
                true
            }
            Self::Os => {
                let Some(entry) = Self::entry(endpoint_id) else { return false };
                match entry.set_password(code) {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("Could not save the connection code: {e}");
                        false
                    }
                }
            }
        }
    }

    /// Retrieve the code for a host, if one was saved and is still readable.
    pub fn load(&self, endpoint_id: &str) -> Option<String> {
        match self {
            Self::Memory(map) => map.lock().unwrap().get(endpoint_id).cloned(),
            Self::Os => {
                let entry = Self::entry(endpoint_id)?;
                match entry.get_password() {
                    Ok(code) => Some(code),
                    // Missing is the normal case for a host never saved.
                    Err(keyring::Error::NoEntry) => None,
                    Err(e) => {
                        debug!("Could not read the saved connection code: {e}");
                        None
                    }
                }
            }
        }
    }

    /// Forget the code for a host.
    ///
    /// Called when a saved connection is removed, so "forget this host" really
    /// does destroy the credential rather than just hiding the row.
    pub fn forget(&self, endpoint_id: &str) {
        match self {
            Self::Memory(map) => {
                map.lock().unwrap().remove(endpoint_id);
            }
            Self::Os => {
                let Some(entry) = Self::entry(endpoint_id) else { return };
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(e) => warn!("Could not delete the saved connection code: {e}"),
                }
            }
        }
    }

    /// Whether a code is stored for this host.
    pub fn has(&self, endpoint_id: &str) -> bool {
        self.load(endpoint_id).is_some()
    }
}

/// Copy one credential from an old service name to this one.
///
/// Returns whether anything was moved. Used only by the rename migration; see
/// [`crate::migrate`] for why the identity in particular cannot be allowed to
/// go missing.
///
/// Never overwrites: a value already present under the new name is what the
/// running application has been using, and is by definition the current one.
pub fn migrate_entry(old_service: &str, old_id: &str, new_id: &str) -> bool {
    let already = keyring::Entry::new(SERVICE, new_id)
        .ok()
        .and_then(|e| e.get_password().ok());
    if already.is_some() {
        return false;
    }
    let Some(value) = keyring::Entry::new(old_service, old_id)
        .ok()
        .and_then(|e| e.get_password().ok())
    else {
        return false;
    };
    match keyring::Entry::new(SERVICE, new_id) {
        Ok(entry) => entry.set_password(&value).is_ok(),
        Err(_) => false,
    }
}

/// Endpoint ids of every saved connection.
///
/// A credential store cannot be listed, so this file is the only record of
/// which per-host codes exist to migrate.
pub fn saved_endpoints() -> Vec<String> {
    super::connections::ConnectionStore::load()
        .entries()
        .iter()
        .map(|c| c.endpoint_id.clone())
        .collect()
}

/// Ids the machine's *own* secrets are filed under.
///
/// Fixed strings rather than something derived from the endpoint id, because
/// the identity key is what *produces* the endpoint id — it has to be findable
/// before there is one.
const IDENTITY_ID: &str = "tain-host-identity";
const MASTER_ID: &str = "tain-host-master-code";
const GUEST_ID: &str = "tain-host-guest-code";
/// The service's copy of which devices may use the admin code.
///
/// Endpoint ids are public keys, not secrets, and the application keeps its
/// copy in a settings file for exactly that reason. This copy is in the
/// credential store all the same, because of what it is *used* for here: it is
/// the list that decides who gets in with the admin passcode on a machine with
/// nobody signed in to ask. A file anybody could append a line to would be a
/// way to grant yourself admin access with a stolen code, and the credential
/// store is writable only by Local System and administrators — who could grant
/// themselves that access anyway.
const TRUSTED_ADMINS_ID: &str = "tain-host-trusted-admins";

/// Suffix that gives a second instance on one machine its own identity.
///
/// # Why this exists
///
/// One machine is one identity, which is the right rule and is why these ids
/// are fixed. It also makes a loopback test impossible: two processes restore
/// the same key, present the same endpoint id to the relay, and connecting to
/// "the host" is then ambiguous between them.
///
/// `TAIN_PROFILE` moves an instance to its own slot in the same
/// credential store — still never on disk in plaintext, just filed separately.
/// Unset, which is every real run, this is empty and the ids are unchanged.
fn profiled(id: &str) -> String {
    match std::env::var("TAIN_PROFILE") {
        Ok(profile) if !profile.is_empty() => format!("{id}-{profile}"),
        _ => id.to_string(),
    }
}

/// This machine's own long-lived secrets: its network identity and its codes.
///
/// Separate from the saved-connection secrets above only in what it keys on.
/// The storage rule is identical and non-negotiable: the credential store or
/// nothing. An identity key on disk in plaintext would let anyone who reads the
/// user's config directory impersonate their machine to everyone who has ever
/// saved a connection to it.
///
/// ## Why the identity is here at all
///
/// A connection code embeds the endpoint id, and the endpoint id is the public
/// half of the identity key. iroh mints a fresh keypair on every bind unless
/// told otherwise, so persisting the passcode alone would still hand out a
/// different string every launch. "Do not reset my code" only means anything if
/// both halves survive.
#[derive(Debug)]
pub struct HostSecrets {
    store: SecretStore,
}

impl Default for HostSecrets {
    /// In-memory — see [`SecretStore::default`] for why the safe direction wins.
    fn default() -> Self {
        Self::in_memory()
    }
}

impl HostSecrets {
    /// The real credential store. Production only.
    pub fn os() -> Self {
        Self { store: SecretStore::Os }
    }

    /// A store that never touches the operating system. Tests only.
    pub fn in_memory() -> Self {
        Self { store: SecretStore::in_memory() }
    }

    fn code_id(typ: tain_core::PasscodeType) -> String {
        match typ {
            tain_core::PasscodeType::Master => profiled(MASTER_ID),
            tain_core::PasscodeType::Guest => profiled(GUEST_ID),
        }
    }

    /// The saved network identity, if there is a usable one.
    pub fn identity(&self) -> Option<[u8; 32]> {
        let hex = self.store.load(&profiled(IDENTITY_ID))?;
        match decode_key(&hex) {
            Some(key) => Some(key),
            None => {
                // Corrupt rather than absent. Say so — the visible symptom is a
                // changed endpoint id, which otherwise looks like nothing at all.
                warn!("Saved host identity is unreadable; starting with a new one");
                None
            }
        }
    }

    /// Save the network identity for the next run. Returns whether it stuck.
    pub fn set_identity(&self, key: &[u8; 32]) -> bool {
        self.store.store(&profiled(IDENTITY_ID), &encode_key(key))
    }

    /// Destroy the saved network identity.
    ///
    /// For the service, when unattended access is switched off. The message
    /// that asks for it is called `ForgetIdentity` and says forgetting is the
    /// point — that a host key left in Local System's store after somebody
    /// declined the feature is a cost they did not agree to. It forgot the
    /// codes and kept the key, which is the one part that matters most.
    pub fn forget_identity(&self) {
        self.store.forget(&profiled(IDENTITY_ID));
    }

    /// Which devices the application has approved for the admin passcode.
    ///
    /// # Why the service needs its own copy
    ///
    /// The check that gates the admin passcode is a set of endpoint ids, and
    /// the application keeps that set in its settings file, under the signed-in
    /// user's profile. Local System cannot read another account's profile — the
    /// same wall that made the identity have to be handed over. So a service
    /// hosting on this machine's behalf started with an empty set and refused
    /// every admin connection, including from devices the owner had already
    /// approved at this very machine. The code was right, the endpoint was
    /// approved, and the answer was still no.
    ///
    /// Anything unreadable is dropped rather than refused, matching the
    /// application: a malformed entry means one device gets asked about again,
    /// which is the safe direction.
    pub fn trusted_admins(&self) -> std::collections::BTreeSet<[u8; 32]> {
        let Some(stored) = self.store.load(&profiled(TRUSTED_ADMINS_ID)) else {
            return std::collections::BTreeSet::new();
        };
        stored.split(',').filter_map(|hex| decode_key(hex.trim())).collect()
    }

    /// Replace the approved set. Returns whether it stuck.
    ///
    /// Replace rather than add, because the application's list is the whole
    /// truth and revocation has to travel: resetting the admin passcode empties
    /// it there, and a service that only ever learned additions would go on
    /// trusting a device the owner had just revoked.
    pub fn set_trusted_admins(&self, endpoints: &std::collections::BTreeSet<[u8; 32]>) -> bool {
        let joined =
            endpoints.iter().map(encode_key).collect::<Vec<_>>().join(",");
        // An empty set is stored as an empty string rather than forgotten, so
        // "approved nobody" and "never told" stay distinguishable.
        self.store.store(&profiled(TRUSTED_ADMINS_ID), &joined)
    }

    /// Destroy the approved set, when unattended access is switched off.
    pub fn forget_trusted_admins(&self) {
        self.store.forget(&profiled(TRUSTED_ADMINS_ID));
    }

    /// The saved code of this type, verbatim as it was handed out.
    ///
    /// Codes are stored whole rather than as bare secrets so the round trip goes
    /// through [`tain_core::ConnectionCode::parse`], which checksums. A corrupt
    /// entry fails to parse instead of adopting 32 bytes of garbage as a
    /// working passcode.
    pub fn code(&self, typ: tain_core::PasscodeType) -> Option<String> {
        self.store.load(&Self::code_id(typ))
    }

    /// Save the code of this type, replacing any previous one.
    pub fn set_code(&self, typ: tain_core::PasscodeType, code: &str) -> bool {
        self.store.store(&Self::code_id(typ), code)
    }

    /// Destroy the saved code of this type.
    ///
    /// Called when auto-reset is on: leaving yesterday's code in the credential
    /// store when the user has asked for a new one every launch keeps a working
    /// secret alive well past its welcome.
    pub fn forget_code(&self, typ: tain_core::PasscodeType) {
        self.store.forget(&Self::code_id(typ));
    }
}

fn encode_key(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_key(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_round_trips_and_forgetting_destroys_it() {
        let store = SecretStore::in_memory();
        assert!(store.store("host-a", "GUEST-abc123"));
        assert_eq!(store.load("host-a").as_deref(), Some("GUEST-abc123"));
        assert!(store.has("host-a"));

        store.forget("host-a");
        assert!(store.load("host-a").is_none(), "forgetting must destroy the credential");
        assert!(!store.has("host-a"));
    }

    #[test]
    fn storing_twice_replaces_rather_than_duplicates() {
        let store = SecretStore::in_memory();
        store.store("host-a", "first");
        store.store("host-a", "second");
        assert_eq!(store.load("host-a").as_deref(), Some("second"));
    }

    #[test]
    fn an_unknown_host_has_no_code_and_does_not_error() {
        let store = SecretStore::in_memory();
        assert!(store.load("never-stored").is_none());
        assert!(!store.has("never-stored"));
        // Forgetting something never stored is a no-op, not a failure.
        store.forget("never-stored");
    }

    #[test]
    fn an_in_memory_store_cannot_reach_the_operating_system() {
        // The point of the split. If this ever becomes constructible as `Os` by
        // accident, test runs start leaving real credentials behind — which is
        // exactly what happened before the type carried the decision.
        assert!(matches!(SecretStore::in_memory(), SecretStore::Memory(_)));
        // The default must be the harmless one — see the impl.
        assert!(matches!(SecretStore::default(), SecretStore::Memory(_)));
    }

    #[test]
    fn an_identity_key_survives_the_round_trip_byte_for_byte() {
        // Every byte matters: a key that comes back one bit different produces a
        // different endpoint id, which silently invalidates every code handed out.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(0x8f);
        }
        let secrets = HostSecrets::in_memory();
        assert!(secrets.set_identity(&key));
        assert_eq!(secrets.identity(), Some(key));
    }

    #[test]
    fn a_corrupt_identity_reads_as_absent_rather_than_as_garbage() {
        let secrets = HostSecrets::in_memory();
        for junk in ["", "not hex", "ab", &"zz".repeat(32)] {
            secrets.store.store(IDENTITY_ID, junk);
            assert_eq!(secrets.identity(), None, "{junk:?} must not decode");
        }
    }

    #[test]
    fn the_two_code_types_do_not_share_a_slot() {
        use tain_core::PasscodeType::*;
        let secrets = HostSecrets::in_memory();
        secrets.set_code(Master, "MASTER-aaa");
        secrets.set_code(Guest, "GUEST-bbb");
        assert_eq!(secrets.code(Master).as_deref(), Some("MASTER-aaa"));
        assert_eq!(secrets.code(Guest).as_deref(), Some("GUEST-bbb"));

        // Forgetting one must not take the other with it.
        secrets.forget_code(Guest);
        assert_eq!(secrets.code(Master).as_deref(), Some("MASTER-aaa"));
        assert_eq!(secrets.code(Guest), None);
    }

    /// The real credential store, exercised once and cleaned up.
    ///
    /// Ignored by default: it writes to the developer's own Credential Manager,
    /// and the rest of the suite must never do that.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn the_os_backend_round_trips() {
        let store = SecretStore::Os;
        let id = "tain-selftest-endpoint";
        store.forget(id);

        if !store.store(id, "GUEST-selftest") {
            eprintln!("no credential store on this machine — skipping");
            return;
        }
        assert_eq!(store.load(id).as_deref(), Some("GUEST-selftest"));

        store.forget(id);
        assert!(store.load(id).is_none());
    }
}
