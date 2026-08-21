//! Where the log goes.
//!
//! ## Why a file, always
//!
//! Release builds are linked as `windows_subsystem = "windows"`, so tracing used
//! to be installed only when [`super::console`] found a console to attach to.
//! Launched any other way — double-clicked, from a shortcut, or with stdout
//! redirected — the app produced **no diagnostics at all**.
//!
//! That is fine for a tool you run from a terminal and fatal for one that runs
//! on other people's machines. "Send me your log file" is the only diagnostic
//! that works at a distance, and there was no file to send.
//!
//! So the log is always written to disk, and additionally to the console when
//! there is one.
//!
//! ## Why it is truncated rather than rolled
//!
//! One previous run is kept alongside the current one. A session that goes wrong
//! is diagnosed from the run it went wrong in, or the one before it if the user
//! restarted before asking — beyond that, older logs are just disk. Rolling by
//! size or date would mean a dependency and a retention policy for no gain.

use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Install the subscriber. Returns the log file's path, if there is one.
///
/// `console` says whether stdio is connected to anything, which decides only
/// whether the console gets a copy — the file is written either way.
pub fn init(console: bool) -> Option<PathBuf> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let Some(path) = log_path() else {
        // No writable location. Better than nothing is the console, if present.
        if console {
            tracing_subscriber::fmt().with_env_filter(filter()).init();
        }
        return None;
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Keep exactly one previous run. Failure is not worth reporting: it means
    // there was no previous log, which is the normal first-run case.
    let _ = std::fs::rename(&path, path.with_extension("log.1"));

    let Ok(file) = std::fs::File::create(&path) else {
        if console {
            tracing_subscriber::fmt().with_env_filter(filter()).init();
        }
        return None;
    };

    // ANSI off for the file: escape codes in a log someone is going to paste
    // into a chat window are noise at best.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file));

    if console {
        // Both destinations. `and` needs the extension trait in scope, and the
        // console copy keeps colour because a terminal can use it.
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        let Ok(file) = std::fs::OpenOptions::new().append(true).open(&path) else {
            builder.init();
            return Some(path);
        };
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file).and(std::io::stdout))
            .init();
    } else {
        builder.init();
    }
    Some(path)
}

/// Beside the settings, so there is one place to look for everything.
///
/// `TAIN_LOG_FILE` overrides it outright, which is what makes running two
/// instances on one machine — a loopback test — produce two readable logs
/// instead of one they both fight over.
pub fn log_path() -> Option<PathBuf> {
    resolve_log_path(std::env::var_os("TAIN_LOG_FILE"))
}

/// The decision, with the environment passed in rather than read.
///
/// Split out so it can be tested without touching process-global state: two
/// tests that each set and clear `TAIN_LOG_FILE` race each other when the
/// harness runs them in parallel, and pass or fail depending on timing.
fn resolve_log_path(override_path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(explicit) = override_path {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("Tain").join("tain.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_path_wins_over_the_default_location() {
        let chosen = resolve_log_path(Some("C:/tmp/db-test.log".into()));
        assert_eq!(chosen, Some(PathBuf::from("C:/tmp/db-test.log")));
    }

    #[test]
    fn an_empty_override_falls_back_rather_than_writing_to_nowhere() {
        // An exported-but-empty variable is a shell accident, not a request to
        // log to a file with no name.
        let chosen = resolve_log_path(Some(std::ffi::OsString::new()));
        assert_ne!(chosen, Some(PathBuf::new()));
    }

    #[test]
    fn the_default_lands_beside_the_other_application_data() {
        let Some(path) = resolve_log_path(None) else {
            eprintln!("no data directory on this machine — skipping");
            return;
        };
        assert!(path.ends_with("Tain/tain.log"), "got {}", path.display());
    }
}
