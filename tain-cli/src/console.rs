//! Console attachment for the Windows GUI subsystem.
//!
//! Release builds are linked as `windows_subsystem = "windows"`, so no console
//! window is created — a GUI app spawning a black terminal beside itself looks
//! broken. That also means `println!` and tracing output go nowhere by default.
//!
//! [`attach`] restores output in the two cases where someone actually wants it:
//!
//! - **Launched from an existing terminal.** `AttachConsole(ATTACH_PARENT_PROCESS)`
//!   borrows the parent's console, so `tain.exe` run from PowerShell prints
//!   where you would expect, without ever creating a window of its own.
//! - **`--console`, or `TAIN_CONSOLE=1`.** Allocates a fresh console for
//!   diagnosing a double-clicked build.
//!
//! Debug builds keep the console subsystem, so none of this applies to them.

/// Whether the user explicitly asked for a console window.
pub fn requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--console")
        || std::env::var("TAIN_CONSOLE").is_ok_and(|v| v == "1")
}

/// Attach to a parent console, or allocate one if explicitly requested.
///
/// Returns whether stdio is now connected to something.
pub fn attach(force: bool) -> bool {
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    {
        use windows::Win32::System::Console::{
            AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS,
        };

        // SAFETY: both calls take no pointers and are safe to fail.
        let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok();
        if attached {
            return true;
        }
        if force {
            // SAFETY: as above.
            return unsafe { AllocConsole() }.is_ok();
        }
        false
    }

    #[cfg(not(all(target_os = "windows", not(debug_assertions))))]
    {
        // Debug builds, and every non-Windows target, already own a console.
        let _ = force;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_console_flag_is_recognised() {
        assert!(requested(&args(&["tain", "--console"])));
        assert!(!requested(&args(&["tain"])));
        assert!(!requested(&args(&["tain", "--consol"])));
    }

    #[test]
    fn debug_builds_always_report_a_console() {
        // Debug keeps the console subsystem, so attaching is a no-op success.
        #[cfg(debug_assertions)]
        assert!(attach(false));
    }
}
