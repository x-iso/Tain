//! Passcode generation and validation.
//!
//! Two passcode types:
//! - **Master** — the admin code: every screen, mouse and keyboard. The first
//!   connection from each device still has to be allowed at the host, and is
//!   trusted after that
//! - **Guest** — nothing at all, not even the screen, until the host allows the
//!   connection, and that is asked again every time
//!
//! Who has to be asked is [`crate::network::must_be_approved`].
//!
//! This module owns only the *secret*. The shareable string a user copies is a
//! [`crate::connection_code::ConnectionCode`], which wraps a secret together
//! with the host's endpoint ID — the store has no idea what address it is
//! reachable at, and does not need one to authenticate.
//!
//! Approval state deliberately does not live here either. A `GUEST-` code is
//! shared with everyone the host hands it to, so approval keyed on the secret
//! would apply to every guest holding that code at once. Per-connection
//! approval is tracked in [`crate::session::SessionManager`].

use anyhow::Result;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Authorization level encoded into a passcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PasscodeType {
    /// Master: full control, once this device has been approved at the host.
    Master,
    /// Guest: streamed nothing until the host allows this connection.
    Guest,
}

impl PasscodeType {
    pub fn prefix(&self) -> &'static str {
        match self {
            Self::Master => "MASTER",
            Self::Guest => "GUEST",
        }
    }

    /// Parse a passcode type from the prefix of an encoded string.
    pub fn from_prefix(s: &str) -> Option<Self> {
        match s.trim().to_uppercase().as_str() {
            "MASTER" => Some(Self::Master),
            "GUEST" => Some(Self::Guest),
            _ => None,
        }
    }
}

/// Redact a connection code for logging.
///
/// Full codes must never reach a log sink — a master code is unattended full
/// control of the machine.
pub fn redact(encoded: &str) -> String {
    match encoded.split_once('-') {
        Some((prefix, body)) if body.len() > 4 => format!("{prefix}-…{}", &body[body.len() - 4..]),
        _ => "…".to_string(),
    }
}

/// The set of secrets currently accepted by this host.
pub struct PasscodeStore {
    secrets: Arc<Mutex<HashMap<[u8; 32], PasscodeType>>>,
}

impl Default for PasscodeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PasscodeStore {
    pub fn new() -> Self {
        Self { secrets: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Generate and register a new secret of the given type.
    pub async fn generate(&self, typ: PasscodeType) -> Result<[u8; 32]> {
        // Fill the secret synchronously so the RNG isn't held across an await point.
        let mut secret = [0u8; 32];
        rand::rng().fill_bytes(&mut secret);

        self.secrets.lock().await.insert(secret, typ);
        Ok(secret)
    }

    /// Register a secret that already exists, rather than inventing one.
    ///
    /// This is what lets a code survive a restart. Without it every launch
    /// invents new secrets, so a code shared with someone yesterday stops
    /// working today even with auto-reset switched off — which is not what
    /// "do not reset" means to anyone.
    pub async fn adopt(&self, secret: [u8; 32], typ: PasscodeType) {
        self.secrets.lock().await.insert(secret, typ);
    }

    /// Replace every secret of `typ` with a freshly generated one.
    ///
    /// Used for auto-reset: the old code stops working the moment the new one exists.
    pub async fn regenerate(&self, typ: PasscodeType) -> Result<[u8; 32]> {
        self.secrets.lock().await.retain(|_, t| *t != typ);
        self.generate(typ).await
    }

    /// Look up a secret's authorization level, if it is currently valid.
    ///
    /// Lookup is not constant-time. With 256 bits of entropy per secret that is
    /// not a practical distinguisher, but revisit if secrets ever get shorter.
    pub async fn validate(&self, secret: &[u8; 32]) -> Option<PasscodeType> {
        self.secrets.lock().await.get(secret).copied()
    }

    /// Every currently valid secret, for display or management.
    pub async fn list_codes(&self) -> Vec<([u8; 32], PasscodeType)> {
        self.secrets.lock().await.iter().map(|(s, t)| (*s, *t)).collect()
    }

    /// Invalidate a single secret.
    pub async fn revoke(&self, secret: &[u8; 32]) -> Result<()> {
        if self.secrets.lock().await.remove(secret).is_some() {
            Ok(())
        } else {
            anyhow::bail!("Passcode not found")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generated_secrets_validate_with_their_type() {
        let store = PasscodeStore::new();
        let master = store.generate(PasscodeType::Master).await.unwrap();
        let guest = store.generate(PasscodeType::Guest).await.unwrap();

        assert_eq!(store.validate(&master).await, Some(PasscodeType::Master));
        assert_eq!(store.validate(&guest).await, Some(PasscodeType::Guest));
    }

    #[tokio::test]
    async fn unknown_secret_does_not_validate() {
        let store = PasscodeStore::new();
        assert_eq!(store.validate(&[0u8; 32]).await, None);
    }

    #[tokio::test]
    async fn revoking_invalidates_immediately() {
        let store = PasscodeStore::new();
        let secret = store.generate(PasscodeType::Master).await.unwrap();

        store.revoke(&secret).await.unwrap();
        assert_eq!(store.validate(&secret).await, None);
        assert!(store.revoke(&secret).await.is_err(), "second revoke should fail");
    }

    #[tokio::test]
    async fn secrets_are_unique() {
        let store = PasscodeStore::new();
        let a = store.generate(PasscodeType::Master).await.unwrap();
        let b = store.generate(PasscodeType::Master).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn regenerate_invalidates_the_previous_code_of_that_type_only() {
        let store = PasscodeStore::new();
        let master = store.generate(PasscodeType::Master).await.unwrap();
        let old_guest = store.generate(PasscodeType::Guest).await.unwrap();

        let new_guest = store.regenerate(PasscodeType::Guest).await.unwrap();

        assert_eq!(store.validate(&old_guest).await, None, "old guest code must stop working");
        assert_eq!(store.validate(&new_guest).await, Some(PasscodeType::Guest));
        assert_eq!(store.validate(&master).await, Some(PasscodeType::Master), "master untouched");
    }

    #[test]
    fn prefixes_parse_case_insensitively_and_reject_junk() {
        assert_eq!(PasscodeType::from_prefix("MASTER"), Some(PasscodeType::Master));
        assert_eq!(PasscodeType::from_prefix("guest"), Some(PasscodeType::Guest));
        assert_eq!(PasscodeType::from_prefix("MASTERY"), None, "must not match by prefix alone");
        assert_eq!(PasscodeType::from_prefix(""), None);
    }

    #[test]
    fn redact_keeps_only_the_prefix_and_a_short_tail() {
        let r = redact("MASTER-aB3kR9mNpQwXyZcDeFgHiJkL");
        assert!(r.starts_with("MASTER-"));
        assert!(!r.contains("aB3kR9"));
        assert_eq!(redact("garbage"), "…");
    }
}
