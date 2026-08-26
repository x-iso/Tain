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
//! ## How many previous runs are kept
//!
//! Eight. It was one, on the reasoning that a session which goes wrong is
//! diagnosed from the run it went wrong in, or the one before it.
//!
//! That reasoning does not survive contact with how this is actually used.
//! Noticing a problem and asking about it are not the same event: a laggy
//! session gets reported after it ends, and by then the application has been
//! restarted to try again, or to install the service, or because it was
//! closed for the night. Each of those is a launch, and with one slot every
//! launch destroys the evidence one further back.
//!
//! It has happened repeatedly, and cost a whole investigation the day this
//! was changed — the log of the session being asked about had already been
//! overwritten by the relaunch that followed it. Eight runs of a log that is
//! tens of kilobytes is nothing; losing the one run somebody is asking about
//! is expensive every time.

use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// How many previous runs to keep beside the current log.
const KEEP: u32 = 8;

/// Shuffle the previous runs along, oldest falling off the end.
///
/// Renaming from the oldest backwards means each slot is free before anything
/// moves into it. Going the other way would overwrite `log.2` with `log.1`
/// before `log.2` had moved, and quietly keep two copies of the same run.
///
/// Every failure here is ignored on purpose: the usual one is "there was no
/// previous log", which is what a first run looks like, and none of them are
/// worth refusing to start over.
fn rotate(path: &std::path::Path) {
    let slot = |n: u32| path.with_extension(format!("log.{n}"));

    let _ = std::fs::remove_file(slot(KEEP));
    for n in (1..KEEP).rev() {
        let _ = std::fs::rename(slot(n), slot(n + 1));
    }
    let _ = std::fs::rename(path, slot(1));
}

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
    rotate(&path);

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

/// Where a background process writes, given a name for it.
///
/// # Why these do not go where the application's log goes
///
/// The service runs as Local System, whose `APPDATA` is inside
/// `C:\Windows\System32\config\systemprofile`. Nothing the signed-in user
/// runs can read that without elevating, which makes the one component that
/// cannot be attached to a console also the one whose log nobody can open. The
/// first time the service did anything worth watching, neither of us could see
/// whether it had.
///
/// The agent has the opposite problem and it is worse. It runs as the console
/// user, so it lands on exactly the `tain.log` the application uses — and that
/// file is truncated at startup, so an agent and a window opened in either
/// order quietly destroy each other's evidence.
///
/// `%ProgramData%\Tain` is the answer to both: one place, writable by Local
/// System, readable by the user without elevating, and one file per component
/// so nothing shares. Named per desktop for the agent, because there can be an
/// agent on `Default` and another on `Winlogon` within the same second and
/// which one said what is the entire question.
pub fn component_log_path(component: &str) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("TAIN_LOG_FILE") {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    // Sanitised because it reaches here as a desktop name, which is chosen by
    // whoever created the desktop rather than by us.
    let safe: String = component
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Some(base.join("Tain").join(format!("{safe}.log")))
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

    /// Eight runs are kept, and the oldest falls off rather than surviving
    /// forever or being overwritten by a younger one.
    #[test]
    fn rotation_shuffles_the_previous_runs_along() {
        let dir = std::env::temp_dir().join(format!("tain-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("tain.log");

        // Nine launches, each writing which run it was.
        for run in 1..=9 {
            std::fs::write(&path, format!("run {run}")).expect("writable");
            rotate(&path);
        }

        // `log.1` is the run before last, and `log.8` the oldest kept.
        for (slot, run) in (1..=KEEP).zip((1..=9u32).rev()) {
            let found = std::fs::read_to_string(path.with_extension(format!("log.{slot}")))
                .unwrap_or_default();
            assert_eq!(found, format!("run {run}"), "slot {slot} holds the wrong run");
        }
        // Nine launches, eight slots: the first run is gone rather than kept.
        assert!(!path.with_extension(format!("log.{}", KEEP + 1)).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_component_log_is_named_after_the_component() {
        let Some(path) = component_log_path("service") else {
            eprintln!("no data directory on this machine — skipping");
            return;
        };
        assert!(path.ends_with("Tain/service.log"), "got {}", path.display());
    }

    /// A desktop name arrives from the system, not from us.
    #[test]
    fn a_component_name_cannot_escape_its_directory() {
        let Some(path) = component_log_path("../../evil") else {
            eprintln!("no data directory on this machine — skipping");
            return;
        };
        assert!(path.ends_with("Tain/______evil.log"), "got {}", path.display());
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
