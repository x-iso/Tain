//! Tain's own `.desktop` entry and icon, kept in place by Tain itself on Linux.
//!
//! # Why the application writes these rather than the person installing it
//!
//! Three things on a Linux desktop hang off a `.desktop` file named after the
//! window's application id, `tain`, and they are not small:
//!
//! - **The taskbar icon.** A Wayland window cannot hand the desktop an icon of
//!   its own through winit, so the desktop finds the window's `.desktop` file by
//!   its application id and uses the icon named there.
//! - **Sharing windows on Plasma.** KWin keeps its window list, window capture
//!   and fake input from any program whose `.desktop` file does not ask for
//!   them in `X-KDE-Wayland-Interfaces`, and it finds that file by matching the
//!   running binary against the first word of each `Exec=` line, by its full
//!   path. See `tain_core::kwin`.
//! - **The notification's name and icon**, through the `desktop-entry` hint.
//!
//! The release used to ship the file and ask for it to be copied into place.
//! The copy said `Exec=tain`, which names nothing KWin can match; fixing that
//! meant a `sed` line pointed at wherever the binary had been put, which is
//! exactly the sort of step that goes wrong on someone else's machine — and when
//! it did, nothing said so: windows simply fell back to the portal and the
//! taskbar showed a generic icon. The binary knows its own path and is in a far
//! better position to get this right, every time it starts, including after it
//! has been moved.
//!
//! # What it leaves alone
//!
//! A packaged install, whose own `tain.desktop` in a system data directory
//! already names this binary: that is the distribution's to keep. Otherwise
//! the user's `tain.desktop` is Tain's own, written here, and rewritten only
//! when what it should say has changed.

use std::path::{Path, PathBuf};
use tracing::*;

/// The entry the release ships, which this fills in.
const TEMPLATE: &str = include_str!("../../packaging/linux/tain.desktop");

/// The mark, as the taskbar and the notifications show it.
const ICON: &[u8] = include_bytes!("../assets/logo.svg");

/// Put Tain's `.desktop` entry and icon in place, or bring them up to date.
///
/// Before the first window and before anything connects to KWin: both read the
/// entry when they first see this process.
pub fn install() {
    let Some(data) = data_home() else {
        debug!("No home directory to install Tain's desktop entry into");
        return;
    };
    let exe = match std::env::current_exe().and_then(|exe| exe.canonicalize()) {
        Ok(exe) => exe,
        Err(e) => {
            debug!("Not installing a desktop entry: cannot tell where Tain is ({e})");
            return;
        }
    };
    if packaged(&exe) {
        debug!("Tain's desktop entry comes with its package");
        return;
    }

    let icon = data.join("icons/hicolor/scalable/apps/tain.svg");
    if let Err(e) = write_if_changed(&icon, ICON) {
        warn!("Could not install Tain's icon at {}: {e}", icon.display());
    }

    let entry = data.join("applications/tain.desktop");
    let wanted = render(&exe, &icon);
    match write_if_changed(&entry, wanted.as_bytes()) {
        Ok(true) => {
            info!("Installed Tain's desktop entry at {}, starting {}", entry.display(), exe.display());
            refresh_kde_menu();
        }
        Ok(false) => {}
        Err(e) => warn!(
            "Could not install Tain's desktop entry at {} ({e}) — the taskbar will show a generic \
             icon, and on Plasma windows are shared through the portal",
            entry.display()
        ),
    }
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

/// Whether a system data directory already has an entry for this very binary.
fn packaged(exe: &Path) -> bool {
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.split(':').filter(|dir| !dir.is_empty()).any(|dir| {
        let entry = Path::new(dir).join("applications/tain.desktop");
        std::fs::read_to_string(entry)
            .ok()
            .and_then(|text| exec_program(&text))
            .and_then(|program| Path::new(&program).canonicalize().ok())
            .is_some_and(|program| program == exe)
    })
}

/// The shipped entry, with this binary and this icon written in.
fn render(exe: &Path, icon: &Path) -> String {
    TEMPLATE
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                format!("Exec={}", quoted(&exe.to_string_lossy()))
            } else if line.starts_with("Icon=") {
                // By path rather than by name: a name is looked up in the icon
                // theme's caches, which a desktop rebuilds when it pleases, and
                // a path is simply read.
                format!("Icon={}", icon.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A path as one argument of an `Exec=` line, quoted where it has to be.
///
/// The specification's rules: inside double quotes, `"`, `` ` ``, `$` and `\`
/// are escaped with a backslash. A path with none of the reserved characters is
/// written as it is, which is what every desktop reads most reliably.
fn quoted(path: &str) -> String {
    const RESERVED: &[char] =
        &[' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`'];
    if !path.contains(RESERVED) {
        return path.to_string();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The program an `Exec=` line starts, unquoted. The inverse of [`quoted`], for
/// the one argument this needs.
fn exec_program(entry: &str) -> Option<String> {
    let line = entry.lines().find_map(|line| line.strip_prefix("Exec="))?.trim();
    if let Some(rest) = line.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?),
                '"' => return Some(out),
                c => out.push(c),
            }
        }
        None
    } else {
        line.split_whitespace().next().map(str::to_string)
    }
}

/// Write `content` to `path` unless it is there already. Returns whether it
/// wrote.
///
/// Through a temporary file and a rename, so a desktop reading the entry at
/// the moment it is replaced reads the old one or the new one, never half.
fn write_if_changed(path: &Path, content: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|existing| existing == content) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension(format!("tain-{}", std::process::id()));
    std::fs::write(&partial, content)?;
    std::fs::rename(&partial, path)?;
    Ok(true)
}

/// Tell Plasma's application database the entry has changed.
///
/// Plasma notices a new file in its own time, which is usually soon and not
/// always before KWin decides what this process may use — and KWin decides
/// once, when Tain connects. Rebuilding now makes the first run work rather
/// than the second. Elsewhere there is nothing to tell, and the program is
/// not installed.
fn refresh_kde_menu() {
    if !std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_uppercase().contains("KDE") {
        return;
    }
    for program in ["kbuildsycoca6", "kbuildsycoca5"] {
        match std::process::Command::new(program)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                debug!("Could not run {program}: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_names_this_binary_and_its_icon_by_path() {
        let entry = render(Path::new("/home/me/Apps/tain/tain"), Path::new("/home/me/.local/share/icons/tain.svg"));
        assert!(entry.contains("\nExec=/home/me/Apps/tain/tain\n"));
        assert!(entry.contains("\nIcon=/home/me/.local/share/icons/tain.svg\n"));
        assert!(entry.contains("X-KDE-Wayland-Interfaces="), "what KWin reads");
        assert!(entry.contains("StartupWMClass=tain"));
        assert!(entry.ends_with('\n'));
    }

    #[test]
    fn what_kwin_asks_for_is_what_the_entry_says() {
        let line = TEMPLATE.lines().find_map(|line| line.strip_prefix("X-KDE-Wayland-Interfaces=")).unwrap();
        assert_eq!(line, tain_core::kwin::INTERFACES);
    }

    #[test]
    fn a_path_with_spaces_is_quoted_and_read_back_whole() {
        for path in ["/home/me/My Apps/tain", "/opt/a\"b$c\\d/tain", "/usr/bin/tain"] {
            let entry = format!("[Desktop Entry]\nExec={}\n", quoted(path));
            assert_eq!(exec_program(&entry).as_deref(), Some(path), "{entry}");
        }
        assert_eq!(quoted("/usr/bin/tain"), "/usr/bin/tain", "nothing to quote, nothing added");
    }

    #[test]
    fn an_unchanged_file_is_not_written_again() {
        let dir = std::env::temp_dir().join(format!("tain-entry-{}", std::process::id()));
        let path = dir.join("applications/tain.desktop");
        assert!(write_if_changed(&path, b"one").unwrap());
        assert!(!write_if_changed(&path, b"one").unwrap());
        assert!(write_if_changed(&path, b"two").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
