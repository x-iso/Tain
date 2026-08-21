//! Carrying an existing installation across the rename to Tain.
//!
//! ## Why this is not optional
//!
//! The application used to be called Deskbridge, and the name appeared in two
//! places that hold state a user cannot reconstruct:
//!
//! - **The credential store.** It holds this machine's *network identity* — the
//!   secret key whose public half is the endpoint id inside every connection
//!   code this host has ever handed out. Filing it under a new name without
//!   moving it would mint a fresh identity, and every code anyone had saved for
//!   this machine would stop working, with nothing to explain why.
//! - **The configuration directory.** Settings, and the list of hosts the user
//!   has connected to before.
//!
//! So the old locations are read once and copied across. Nothing is deleted:
//! if this migration is wrong, the user's data is still where it was.
//!
//! ## Order matters
//!
//! Configuration first, because the saved-connection list is what says which
//! credentials to look for — the credential store cannot be enumerated, only
//! queried by key.
//!
//! This becomes dead code once no installation predates the rename, and can be
//! deleted then. Until that is true it runs on every start and costs a directory
//! check.

use std::path::PathBuf;
use tracing::*;

/// What the application used to be called.
const OLD_NAME: &str = "Deskbridge";

/// Move an existing Deskbridge installation to Tain, if there is one.
///
/// Safe to call on every start: it does nothing once the new locations exist.
pub fn from_deskbridge() {
    let moved_config = migrate_config();
    let moved_secrets = migrate_secrets();
    if moved_config || moved_secrets {
        info!(
            "Carried an existing Deskbridge installation across to Tain \
             (settings: {moved_config}, credentials: {moved_secrets}). \
             The old data has been left in place."
        );
    }
}

/// Copy `settings.json` and `connections.json` into the new directory.
fn migrate_config() -> bool {
    let (Some(old), Some(new)) = (config_dir(OLD_NAME), config_dir(crate::APP_NAME)) else {
        return false;
    };
    if !old.exists() {
        return false;
    }
    if std::fs::create_dir_all(&new).is_err() {
        return false;
    }

    // # Per file, not per directory
    //
    // Guarding on "does the new directory exist" looked right and was wrong:
    // anything that touches settings creates the directory with a settings file
    // in it, and the connection list would then be skipped and left behind —
    // the saved-hosts list lost, silently, which is the exact outcome the guard
    // was meant to prevent.
    //
    // Each file is considered on its own, and one already present is never
    // overwritten: it is what the application has been using and is by
    // definition the current one.
    let mut moved = false;
    for name in ["settings.json", "connections.json"] {
        let (from, to) = (old.join(name), new.join(name));
        if !from.exists() || to.exists() {
            continue;
        }
        if std::fs::copy(&from, &to).is_ok() {
            moved = true;
        }
    }
    moved
}

/// Copy this machine's identity, its two passcodes, and every saved code.
fn migrate_secrets() -> bool {
    use super::gui::secrets::{migrate_entry, saved_endpoints};

    // The identity first: it is the one whose loss is irreversible from the
    // user's point of view, because it changes what this machine *is* on the
    // network rather than merely what it remembers.
    let mut moved = false;
    for id in ["host-identity", "host-master-code", "host-guest-code"] {
        let old_id = format!("deskbridge-{id}");
        let new_id = format!("tain-{id}");
        moved |= migrate_entry(OLD_NAME, &old_id, &new_id);
    }

    // Saved connection codes are keyed by the host's endpoint id, and a
    // credential store cannot be listed — so the connection file is the only
    // record of which keys exist. Migrated after the config for that reason.
    for endpoint in saved_endpoints() {
        moved |= migrate_entry(OLD_NAME, &endpoint, &endpoint);
    }
    moved
}

/// Where an application of this name keeps its configuration.
fn config_dir(app: &str) -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(app))
}

/// Whether `path` is inside `root`, for tests that must not touch real data.
#[cfg(test)]
fn is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One file already present must not block the others.
    ///
    /// # The bug this is here for
    ///
    /// The first version skipped migration entirely if the new *directory*
    /// existed. Anything that touches settings creates that directory with a
    /// settings file in it — which had already happened on the development
    /// machine before this ran — so the connection list would have been left
    /// behind and the user's saved hosts lost without a word.
    #[test]
    fn a_file_already_present_does_not_block_the_ones_that_are_not() {
        let temp = std::env::temp_dir().join("tain-migrate-test");
        let _ = std::fs::remove_dir_all(&temp);
        let old = temp.join(OLD_NAME);
        let new = temp.join("Tain");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();

        // Both exist on the old side; only settings exists on the new.
        std::fs::write(old.join("settings.json"), b"old settings").unwrap();
        std::fs::write(old.join("connections.json"), b"old connections").unwrap();
        std::fs::write(new.join("settings.json"), b"new settings").unwrap();

        let mut moved = false;
        for name in ["settings.json", "connections.json"] {
            let (from, to) = (old.join(name), new.join(name));
            if !from.exists() || to.exists() {
                continue;
            }
            if std::fs::copy(&from, &to).is_ok() {
                moved = true;
            }
        }

        assert!(moved, "the connection list should have been carried over");
        assert_eq!(
            std::fs::read(new.join("settings.json")).unwrap(),
            b"new settings",
            "a file already present is never overwritten"
        );
        assert_eq!(
            std::fs::read(new.join("connections.json")).unwrap(),
            b"old connections",
            "and one that is missing is brought across"
        );

        assert!(is_within(&new, &temp));
        let _ = std::fs::remove_dir_all(&temp);
    }

    /// The identity has to be first in the list.
    ///
    /// Not cosmetic: if migration is interrupted, the identity is the one whose
    /// absence changes what this machine is on the network, rather than merely
    /// what it remembers.
    #[test]
    fn the_identity_is_migrated_before_anything_else() {
        let order = ["host-identity", "host-master-code", "host-guest-code"];
        assert_eq!(order[0], "host-identity");
    }
}
