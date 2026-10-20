//! `tain://` links: what opens Tain from one, and how one reaches the Tain
//! that is already running.
//!
//! A link is a connection code with a scheme in front — see
//! `tain_core::connection_code` — so everything here is about delivery.
//!
//! # What happens when a link is clicked
//!
//! Windows looks the scheme up under the user's classes, finds this executable,
//! and starts it with the link as its argument. That is a new process every
//! time, and a second Tain is not what anybody wants: it would host on the same
//! identity as the first — two machines with one name, as far as every relay is
//! concerned — and open a second main window beside the one already there.
//!
//! So the new process asks first. The running application listens on a named
//! pipe, and a process started with a link hands the link over and exits before
//! it has opened a window, bound an endpoint or said anything to the service.
//! Only when nothing answers does it carry on and become the application,
//! opening the link itself once it is up.
//!
//! # Who may hand a link over, and to whom
//!
//! A link carries a passcode, so neither end takes the other on trust. The pipe
//! admits only this user (see `tain_core::pipe_security`), and the sender
//! checks that the pipe is served from its own session before writing to it: a
//! pipe name is anybody's to create, and one made first by another account
//! would otherwise collect every code clicked on this desktop.
//!
//! # Registration
//!
//! Under `HKEY_CURRENT_USER\Software\Classes`, which needs no administrator and
//! is this user's alone. Rewritten at launch whenever it names a different
//! executable, because Tain is a portable program: the copy that was registered
//! may have moved, or been replaced by a newer one somewhere else, and the copy
//! being run is the one its user means.
//!
//! Two kinds of run leave it alone. A debug build, so `cargo run` does not take
//! links away from the copy somebody actually uses — `TAIN_DEBUG=links` lets
//! one register anyway. And a `TAIN_PROFILE` instance, which is a second
//! identity for testing and not where a clicked link should go.

/// The link this process was started with, if it was started by one.
///
/// Windows passes the link as it was clicked, as one argument. Anything else on
/// the command line is a mode flag, and none of those is a link.
pub fn from_args(args: &[String]) -> Option<String> {
    args.iter()
        .skip(1)
        .find(|arg| tain_core::connection_code::code_in_link(arg).is_some())
        .cloned()
}

/// Whether this copy may make itself the one links open. See the module notes.
fn may_register() -> bool {
    if std::env::var("TAIN_PROFILE").is_ok_and(|profile| !profile.is_empty()) {
        return false;
    }
    !cfg!(debug_assertions) || crate::gui::app::debug_flag("links")
}

/// The command Windows runs for a link: this executable, given the link.
fn command_for(exe: &std::path::Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// The longest link worth reading off the pipe. A code with a relay in it is a
/// couple of hundred bytes; this is room for a long relay name and then some.
const MAX_LINK: usize = 4096;

#[cfg(target_os = "windows")]
pub use platform::{hand_to_running, listen, sync_registration};

/// Links are a Windows registration; elsewhere there is nothing to hand over.
#[cfg(not(target_os = "windows"))]
pub fn hand_to_running(_link: &str) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
pub fn listen(_on_link: impl Fn(String) + Send + 'static) {}

#[cfg(not(target_os = "windows"))]
pub fn sync_registration(_wanted: bool) {}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::{bail, Context, Result};
    use tain_core::pipe_security::{console_user_sid, wide, OwnedHandle, PipeSecurity};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, GENERIC_WRITE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_READ_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
        PIPE_ACCESS_INBOUND,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
        GetNamedPipeServerSessionId, WaitNamedPipeW, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ,
    };

    /// Where the scheme is registered, for this user only.
    const CLASS_KEY: &str = r"Software\Classes\tain";
    /// The value Windows reads to start a program for a link.
    const COMMAND_KEY: &str = r"Software\Classes\tain\shell\open\command";

    /// The session this process runs in.
    fn own_session() -> Result<u32> {
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::GetCurrentProcessId;

        let mut session = 0u32;
        // SAFETY: a documented query about this process, into a local.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
            .context("could not tell which session this is")?;
        Ok(session)
    }

    /// One pipe per session: two people signed in are two applications.
    fn pipe_name() -> Result<String> {
        Ok(format!(r"\\.\pipe\tain-links-{}", own_session()?))
    }

    /// Give `link` to the Tain already running here. `false` when there is
    /// none, which is the cue for this process to become it.
    pub fn hand_to_running(link: &str) -> bool {
        match try_hand_over(link) {
            Ok(handed) => handed,
            Err(e) => {
                tracing::warn!("Could not hand the link to the running Tain ({e:#}) — opening it here");
                false
            }
        }
    }

    fn try_hand_over(link: &str) -> Result<bool> {
        let name = wide(&pipe_name()?);

        // One link at a time is read, so a second click arriving while the
        // first is being read finds the pipe busy for a moment, not absent.
        let mut busy = 0;
        let pipe = loop {
            // SAFETY: the name outlives the call. Read-attributes as well as
            // write, because asking who serves the pipe needs it.
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            };
            match opened {
                Ok(handle) => break OwnedHandle(handle),
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() && busy < 3 => {
                    busy += 1;
                    // SAFETY: the name outlives the call.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1000) };
                }
                // Nothing is listening: no Tain is running in this session.
                Err(_) => return Ok(false),
            }
        };

        // # Only to a Tain in this session
        //
        // The access list keeps other accounts from *reading* links sent to
        // our pipe, but not from creating a pipe of the same name before ours
        // exists. Asking where the server runs is what tells the two apart.
        let mut server_session = 0u32;
        // SAFETY: a live pipe handle and a local to write into.
        unsafe { GetNamedPipeServerSessionId(pipe.0, &mut server_session) }
            .context("could not ask who is listening")?;
        if server_session != own_session()? {
            bail!("the pipe is served from session {server_session}, not this one");
        }

        // The click gave this process the right to bring a window forward; the
        // window that should come forward is the other one's.
        let mut server = 0u32;
        // SAFETY: as above.
        if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server) }.is_ok() {
            // SAFETY: a documented call taking a process id.
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(server)
            };
        }

        let bytes = link.trim().as_bytes();
        let mut sent = 0usize;
        while sent < bytes.len() {
            let mut wrote = 0u32;
            // SAFETY: a live pipe handle and a slice that outlives the call.
            unsafe { WriteFile(pipe.0, Some(&bytes[sent..]), Some(&mut wrote), None) }
                .context("the running Tain stopped reading")?;
            if wrote == 0 {
                bail!("the running Tain stopped reading");
            }
            sent += wrote as usize;
        }
        // Until the other end has read it all: this process exits next, and
        // the link must not go with it.
        // SAFETY: a live pipe handle.
        let _ = unsafe { FlushFileBuffers(pipe.0) };
        Ok(true)
    }

    /// Hand every link sent to this application to `on_link`, from a thread of
    /// its own, for as long as the process runs.
    ///
    /// Quiet when the pipe cannot be made. Links then open a second Tain, which
    /// is a nuisance and not a fault — and a `TAIN_PROFILE` instance does not
    /// listen at all, so the main one gets them.
    pub fn listen(on_link: impl Fn(String) + Send + 'static) {
        if std::env::var("TAIN_PROFILE").is_ok_and(|profile| !profile.is_empty()) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("tain-links".into())
            .spawn(move || {
                if let Err(e) = serve(&on_link) {
                    tracing::warn!("Not listening for links ({e:#}) — a clicked link opens another Tain");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("Not listening for links ({e})");
        }
    }

    fn serve(on_link: &dyn Fn(String)) -> Result<()> {
        let name = wide(&pipe_name()?);
        let security = PipeSecurity::for_console_user(&console_user_sid()?)?;

        // # One pipe, kept for the life of the process
        //
        // Reused between clients rather than made anew for each, because the
        // moment between closing one and creating the next is a moment the name
        // is free for anybody else to take. `FIRST_PIPE_INSTANCE` refuses to
        // start at all if somebody already has it.
        //
        // SAFETY: the name and attributes outlive the call.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                MAX_LINK as u32,
                0,
                Some(security.attributes()),
            )
        };
        if pipe.is_invalid() {
            bail!("could not create the pipe: {}", windows::core::Error::from_win32());
        }
        let pipe = OwnedHandle(pipe);
        tracing::debug!("Listening for links");

        let mut failures = 0;
        loop {
            // SAFETY: a pipe this function owns, disconnected after each client.
            let connected = unsafe { ConnectNamedPipe(pipe.0, None) };
            if let Err(e) = &connected
                && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
            {
                // Not expected to happen, and a loop that spun on it would eat
                // a core for the life of the application.
                failures += 1;
                if failures > 10 {
                    bail!("the pipe keeps failing: {e}");
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            failures = 0;

            let received = read_to_end(pipe.0);
            // SAFETY: connected above.
            let _ = unsafe { DisconnectNamedPipe(pipe.0) };
            match received.map(String::from_utf8) {
                Some(Ok(link)) if !link.trim().is_empty() => on_link(link.trim().to_string()),
                Some(Ok(_)) => {}
                Some(Err(_)) => tracing::warn!("A link arrived that was not text — ignored"),
                None => tracing::warn!("A link arrived longer than any link — ignored"),
            }
        }
    }

    /// Everything the client wrote before hanging up, or `None` if it would
    /// not stop.
    fn read_to_end(pipe: windows::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
        let mut received = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let mut read = 0u32;
            // SAFETY: a connected pipe and a buffer that outlives the call.
            let got = unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut read), None) };
            if got.is_err() || read == 0 {
                // The client closing its end is how a link ends.
                return Some(received);
            }
            received.extend_from_slice(&chunk[..read as usize]);
            if received.len() > MAX_LINK {
                return None;
            }
        }
    }

    /// Make this copy the one links open, or stop it being — but only ours.
    pub fn sync_registration(wanted: bool) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                tracing::debug!("Not registering links: no path to this executable ({e})");
                return;
            }
        };
        let ours = command_for(&exe);
        let current = registered_command();

        if wanted {
            if !may_register() {
                tracing::debug!("Not registering links from this build — see `links`");
                return;
            }
            if current.as_deref() == Some(ours.as_str()) {
                return;
            }
            match register(&exe, &ours) {
                Ok(()) => tracing::info!("tain:// links now open {}", exe.display()),
                Err(e) => tracing::warn!("Could not register tain:// links: {e:#}"),
            }
        } else if current.as_deref() == Some(ours.as_str()) {
            // Another copy's registration is that copy's to keep. Turning this
            // off here says "not this one", not "no Tain at all".
            let key = wide(CLASS_KEY);
            // SAFETY: the string outlives the call.
            let removed = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) };
            if removed == ERROR_SUCCESS {
                tracing::info!("tain:// links no longer open this copy of Tain");
            } else {
                tracing::warn!("Could not remove the tain:// registration ({})", removed.0);
            }
        }
    }

    /// The command links currently run, whoever registered it.
    fn registered_command() -> Option<String> {
        let key = wide(COMMAND_KEY);
        let mut handle = HKEY::default();
        // SAFETY: the string outlives the call; the handle is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), 0, KEY_READ, &mut handle)
                != ERROR_SUCCESS
            {
                return None;
            }
            let mut buffer = [0u16; 1024];
            let mut size = std::mem::size_of_val(&buffer) as u32;
            // The key's default value, which is what a null name reads.
            let read = RegQueryValueExW(
                handle,
                PCWSTR::null(),
                None,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            );
            let _ = RegCloseKey(handle);
            if read != ERROR_SUCCESS {
                return None;
            }
            let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            Some(String::from_utf16_lossy(&buffer[..end]))
        }
    }

    /// Write the scheme's registration, pointing at `exe`.
    fn register(exe: &std::path::Path, command: &str) -> Result<()> {
        let icon = format!("\"{}\",0", exe.display());
        // What Windows needs to treat `tain:` as a URL scheme at all: a
        // description, the empty "URL Protocol" marker, an icon for prompts
        // that show one, and the command.
        let values: [(&str, &str, &str); 4] = [
            (CLASS_KEY, "", "URL:Tain connection"),
            (CLASS_KEY, "URL Protocol", ""),
            (r"Software\Classes\tain\DefaultIcon", "", &icon),
            (COMMAND_KEY, "", command),
        ];
        for (key, name, value) in values {
            set_string(key, name, value).with_context(|| format!("writing {key}"))?;
        }
        Ok(())
    }

    fn set_string(key: &str, name: &str, value: &str) -> Result<()> {
        let key = wide(key);
        let name = wide(name);
        let value = wide(value);
        let mut handle = HKEY::default();
        // SAFETY: the strings outlive the calls; the handle is closed below.
        unsafe {
            let created = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut handle,
                None,
            );
            if created != ERROR_SUCCESS {
                bail!("could not create the key ({})", created.0);
            }
            // REG_SZ is the UTF-16 bytes with their terminator.
            let bytes = std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2);
            let written = RegSetValueExW(handle, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(handle);
            if written != ERROR_SUCCESS {
                bail!("could not set the value ({})", written.0);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Windows passes the clicked link as the one argument after the program.
    #[test]
    fn the_link_is_found_among_the_arguments() {
        let link = "tain://connect/GUEST-abc";
        assert_eq!(from_args(&args(&["tain.exe", link])).as_deref(), Some(link));
        assert_eq!(from_args(&args(&["tain.exe", "--console", link])).as_deref(), Some(link));
    }

    /// No link, and the program's own path never counts as one.
    #[test]
    fn a_launch_without_a_link_has_none() {
        assert_eq!(from_args(&args(&["tain.exe"])), None);
        assert_eq!(from_args(&args(&["tain.exe", "--console"])), None);
        assert_eq!(from_args(&args(&["tain://connect/GUEST-abc"])), None);
    }

    /// Quoted, so a path with spaces in it is one program and not three.
    #[test]
    fn the_command_quotes_the_program_and_the_link() {
        let command = command_for(std::path::Path::new(r"C:\Program Files\Tain\tain.exe"));
        assert_eq!(command, r#""C:\Program Files\Tain\tain.exe" "%1""#);
    }

    /// A link handed over arrives whole, and so does the next one, through the
    /// same pipe.
    ///
    /// Ignored because the pipe is this session's: with a Tain running here it
    /// is that Tain's, and the test would hand it two links to open.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "uses this session's link pipe, which a running Tain owns"]
    fn a_link_handed_over_arrives_whole() {
        let (tx, rx) = std::sync::mpsc::channel();
        listen(move |link| {
            let _ = tx.send(link);
        });
        // The listener makes its pipe on its own thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let first = "tain://connect/GUEST-first";
        while !hand_to_running(first) {
            assert!(std::time::Instant::now() < deadline, "nothing ever listened");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let wait = std::time::Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(wait).as_deref(), Ok(first));

        let second = "tain://connect/MASTER-second";
        assert!(hand_to_running(second), "the pipe was not there for a second link");
        assert_eq!(rx.recv_timeout(wait).as_deref(), Ok(second));
    }
}
