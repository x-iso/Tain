//! The per-desktop agent: one process, on one desktop, for as long as that
//! desktop is the one receiving input.
//!
//! # Why a separate process at all
//!
//! Windows scopes almost everything about input and display to a *desktop*
//! object, and a process is bound to the one it started on. It cannot move.
//! Screen capture, cursor state, `SendInput` and window enumeration all follow
//! that binding, so a process running on `Default` can neither see nor touch
//! the secure desktop that UAC and the lock screen use — which is exactly the
//! reported gap: a consent prompt appears and the guest sees a frozen picture
//! and cannot click anything.
//!
//! The only way across is a new process started *on* the other desktop, and
//! only something running as SYSTEM can start one there. Hence the service, and
//! hence this: the service watches which desktop is receiving input and keeps
//! exactly one agent alive on it, replacing the agent when that changes.
//!
//! # What runs where
//!
//! On the ordinary desktop the agent runs as the logged-in user, because that
//! is whose session it is capturing and whose profile and audio endpoints it
//! needs. On the secure desktop it runs as SYSTEM, because no user token is
//! permitted to interact with it — this is the boundary UAC exists to enforce,
//! and the service is on the right side of it only because an administrator
//! installed it.
//!
//! # This is the first half
//!
//! Getting a process onto the input desktop, keeping exactly one, and moving it
//! when the desktop changes is where all the platform difficulty lives, so it
//! lands on its own and can be watched in a log before anything depends on it.
//! What the agent then *does* — capture, injection, and a pipe back to whoever
//! is hosting — attaches to a supervisor already known to work.

use anyhow::Result;

/// Runs this executable as the per-desktop agent.
pub const AGENT_FLAG: &str = "--agent";

/// Names the desktop the agent was started on, for its own log line.
pub const DESKTOP_ARG: &str = "--desktop";

/// Read `--desktop <name>` out of the command line, if it is there.
pub fn desktop_from_args(args: &[String]) -> Option<String> {
    let at = args.iter().position(|a| a == DESKTOP_ARG)?;
    args.get(at + 1).cloned()
}

#[cfg(not(target_os = "windows"))]
pub fn run(_desktop: Option<String>) -> Result<()> {
    anyhow::bail!("The Tain agent is Windows-only")
}

#[cfg(target_os = "windows")]
pub use platform::{input_desktop_name as input_desktop_name_for_service, run};

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// How often the agent checks that its desktop is still the live one.
    ///
    /// Frequent enough that a switch to the secure desktop is handed over in a
    /// fraction of a second, cheap enough to be free: two handle operations and
    /// a string compare.
    const WATCH: std::time::Duration = std::time::Duration::from_millis(300);

    static RUNNING: AtomicBool = AtomicBool::new(true);

    /// Run until the desktop this agent was started on stops receiving input.
    ///
    /// Exiting is the whole mechanism. The agent cannot follow a desktop
    /// switch — no process can — so when one happens it leaves, and the
    /// supervisor starts a replacement on the desktop that now has input. That
    /// keeps the interesting decision in one place instead of splitting it
    /// across two processes that would have to agree.
    pub fn run(desktop: Option<String>) -> Result<()> {
        let started_on = desktop.or_else(current_desktop_name).unwrap_or_default();
        tracing::info!(
            "Tain agent running on desktop {started_on} (session {}, {})",
            session_id(),
            if is_system() { "as SYSTEM" } else { "as the logged-in user" },
        );

        while RUNNING.load(Ordering::Relaxed) {
            std::thread::sleep(WATCH);

            let Some(live) = input_desktop_name() else {
                // The input desktop cannot be opened from here, which is what
                // it looks like from the ordinary desktop while the secure one
                // is up. Somebody else's turn.
                tracing::info!("Agent on {started_on} can no longer see the input desktop — exiting");
                return Ok(());
            };
            if !live.eq_ignore_ascii_case(&started_on) {
                tracing::info!("Input moved from {started_on} to {live} — agent exiting");
                return Ok(());
            }
        }
        Ok(())
    }

    /// The name of the desktop this thread is on.
    fn current_desktop_name() -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::GetThreadDesktop;
        use windows::Win32::System::Threading::GetCurrentThreadId;

        // SAFETY: the handle belongs to this thread and must not be closed.
        let handle = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok()?;
        desktop_name(handle)
    }

    /// The name of the desktop currently receiving input, if it can be opened.
    ///
    /// `None` covers the case that matters as much as any name: a process on
    /// the ordinary desktop cannot open the secure one, so the refusal *is* the
    /// signal that something else now owns the screen.
    pub fn input_desktop_name() -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
        };

        // SAFETY: documented; the handle is closed below.
        unsafe {
            let handle =
                OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
            let name = desktop_name(handle);
            let _ = CloseDesktop(handle);
            name
        }
    }

    fn desktop_name(
        handle: windows::Win32::System::StationsAndDesktops::HDESK,
    ) -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

        let mut buffer = [0u16; 128];
        let mut needed = 0u32;
        // SAFETY: a documented read into a buffer we own, sized in bytes.
        unsafe {
            GetUserObjectInformationW(
                windows::Win32::Foundation::HANDLE(handle.0),
                UOI_NAME,
                Some(buffer.as_mut_ptr().cast()),
                std::mem::size_of_val(&buffer) as u32,
                Some(&mut needed),
            )
            .ok()?;
        }
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }

    /// Which Windows session this process is in. Zero means the service session.
    fn session_id() -> u32 {
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::GetCurrentProcessId;

        let mut session = 0u32;
        // SAFETY: our own process id, into a local.
        unsafe {
            let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        }
        session
    }

    /// Whether this process is running as Local System.
    ///
    /// Only for the log line: which identity the agent got decides what it is
    /// allowed to see, and reading it back is cheaper than inferring it from
    /// which desktop it landed on.
    fn is_system() -> bool {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SAFETY: opening and querying our own token, closed at the end.
        unsafe {
            let mut token = Default::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .is_ok();
            let system = ok && {
                let user = &*(buffer.as_ptr() as *const TOKEN_USER);
                is_local_system(user.User.Sid)
            };
            let _ = CloseHandle(token);
            system
        }
    }

    /// Whether `sid` is Local System.
    ///
    /// # Safety
    ///
    /// `sid` must point at a valid SID.
    unsafe fn is_local_system(sid: windows::Win32::Security::PSID) -> bool {
        use windows::Win32::Security::{IsWellKnownSid, WinLocalSystemSid};

        // SAFETY: forwarded from this function's contract. Asking Windows
        // rather than reassembling S-1-5-18 by hand, which is the kind of
        // detail that is wrong once and then wrong forever.
        unsafe { IsWellKnownSid(sid, WinLocalSystemSid).as_bool() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_argument_is_read_when_present() {
        let args = vec![
            "tain.exe".to_string(),
            AGENT_FLAG.to_string(),
            DESKTOP_ARG.to_string(),
            "Winlogon".to_string(),
        ];
        assert_eq!(desktop_from_args(&args).as_deref(), Some("Winlogon"));
    }

    #[test]
    fn a_missing_desktop_argument_is_not_an_error() {
        let args = vec!["tain.exe".to_string(), AGENT_FLAG.to_string()];
        assert_eq!(desktop_from_args(&args), None);
    }

    /// A trailing `--desktop` with nothing after it must not panic.
    #[test]
    fn a_desktop_argument_with_no_value_is_ignored() {
        let args = vec!["tain.exe".to_string(), DESKTOP_ARG.to_string()];
        assert_eq!(desktop_from_args(&args), None);
    }
}
