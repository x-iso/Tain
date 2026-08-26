//! Registering, controlling and running Tain as a Windows service.
//!
//! ## What this is for
//!
//! Some things a remote desktop needs are unreachable from an ordinary
//! application, no matter how much privilege it is given: a UAC prompt renders
//! on a different *desktop*, and a machine nobody has logged into has no user
//! session to run in at all. Both need a process running as LocalSystem that
//! outlives any one login. That is the service.
//!
//! ## What it deliberately is not
//!
//! It is not a second program, and not an installer. `tain.exe` runs exactly as
//! it always has with no service registered — portable, attended, no elevation,
//! nothing to install. Handing the executable to someone and telling them to
//! run it keeps working unchanged. The service is opt-in, switched on from
//! Settings, and removable from the same place.
//!
//! ## Scope of this piece
//!
//! Registration, removal, and reporting which of those states the machine is
//! in. The service starts and stops cleanly and does nothing else yet: the
//! per-desktop agent, the control pipe and the capture handover come next, and
//! land against a service that already installs and runs.

use anyhow::{bail, Context, Result};

/// Service name in the registry. Never shown to anyone.
pub const SERVICE_NAME: &str = "TainHost";
/// Name shown in the Services list.
pub const DISPLAY_NAME: &str = "Tain Host";
/// Description shown beside it.
pub const DESCRIPTION: &str =
    "Lets Tain reach screens an ordinary application cannot: UAC prompts, the \
     lock screen, and a machine with nobody logged in. Removing it leaves the \
     rest of Tain working normally.";

// ── Internal arguments ────────────────────────────────────────
//
// Nobody types these. They exist because Windows leaves no alternative:
//
// - The service control manager starts a service by running a command line, so
//   the process it launches has to be able to tell that it is the service and
//   not the application. That is `RUN_FLAG`.
// - Registering a service needs administrator rights, which the application
//   does not have and should not ask for at every launch. So the button in
//   Settings relaunches this same executable elevated, and the elevated copy
//   needs to know what it was elevated *for*.
//
// Everything a person does happens through the buttons in Settings.

/// Set by the service control manager when it launches the service.
pub const RUN_FLAG: &str = "--run-service";
/// Set on the elevated relaunch that registers the service.
pub const INSTALL_FLAG: &str = "--install-service";
/// Set on the elevated relaunch that removes it.
pub const UNINSTALL_FLAG: &str = "--uninstall-service";

/// What the machine's service situation is.
///
/// Five states rather than a boolean, because the UI is only honest if it can
/// tell them apart — an app that behaves as though unattended access works
/// while the service is stopped is worse than one that never had the feature,
/// since the discovery happens while travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceState {
    /// Never registered on this machine.
    #[default]
    NotInstalled,
    /// Registered and running.
    Running,
    /// Registered but not running, so nothing it provides is available.
    Stopped,
    /// Registered, but pointing at a different executable than this one.
    ///
    /// Happens after the portable exe is moved or replaced, which for an app
    /// people copy around is normal rather than exotic. Reinstalling fixes it,
    /// and saying so beats a feature that quietly does not work.
    Mismatched,
    /// The service database could not be read at all.
    Unknown,
}

impl ServiceState {
    /// Whether the features that need the service are available right now.
    pub fn is_usable(self) -> bool {
        self == Self::Running
    }

    /// One line for the settings panel.
    pub fn summary(self) -> &'static str {
        match self {
            Self::NotInstalled => "Not installed. Admin access needs it.",
            Self::Running => "Installed and running.",
            Self::Stopped => "Installed but not running — unattended access is unavailable.",
            Self::Mismatched => "Installed, but registered against a different copy of Tain.",
            Self::Unknown => "The service database could not be read.",
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::*;

    pub fn state() -> ServiceState {
        ServiceState::NotInstalled
    }
    pub fn request_install() -> Result<()> {
        bail!("the Tain service is Windows-only")
    }
    pub fn request_uninstall() -> Result<()> {
        bail!("the Tain service is Windows-only")
    }
    pub fn install() -> Result<()> {
        bail!("the Tain service is Windows-only")
    }
    pub fn uninstall() -> Result<()> {
        bail!("the Tain service is Windows-only")
    }
    pub fn run() -> Result<()> {
        bail!("the Tain service is Windows-only")
    }
    pub fn start_if_stopped() -> bool {
        false
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, HANDLE};
    use windows::Win32::System::Services::{
        CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
        OpenServiceW, QueryServiceConfigW, QueryServiceStatus, RegisterServiceCtrlHandlerW,
        ChangeServiceConfig2W, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
        QUERY_SERVICE_CONFIGW, SERVICE_CONFIG_DESCRIPTION, SERVICE_DESCRIPTIONW,
        SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE,
        SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_START,
        SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL, SERVICE_QUERY_CONFIG,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
        SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };
    use std::sync::Arc;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::TerminateProcess;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// Null-terminated UTF-16, which is what every call here wants.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes an SCM handle however the surrounding code exits.
    struct ScHandle(SC_HANDLE);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: a handle from OpenSCManagerW/OpenServiceW, closed once.
                unsafe {
                    let _ = CloseServiceHandle(self.0);
                }
            }
        }
    }

    /// This executable's full path, which is what the service registers.
    fn exe_path() -> Result<String> {
        Ok(std::env::current_exe()
            .context("could not find this executable")?
            .to_string_lossy()
            .into_owned())
    }

    /// Where the installed copy of this executable lives.
    ///
    /// # Why the service does not run the binary it was installed from
    ///
    /// It used to, and a running service holds its image open. So the service
    /// pinned whatever file it was installed from, and on the machine this is
    /// developed on that is the build output — every rebuild failed with
    /// "Access is denied" until the service was stopped by hand, which needs
    /// administrator rights, which means a prompt, several times an hour.
    ///
    /// Copying is also what an install *should* do. A service registered
    /// against a file in someone's downloads folder, or a build directory, or a
    /// removable disk, is a service that stops existing when that file moves.
    /// `%ProgramData%\Tain` is where the service's own logs already live and it
    /// survives the executable being moved, replaced or deleted.
    ///
    /// Reinstalling copies afresh, so picking up a new build is the same
    /// gesture it always was.
    fn installed_exe_path() -> Result<std::path::PathBuf> {
        let base = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .context("no ProgramData directory on this machine")?;
        Ok(base.join("Tain").join("tain-service.exe"))
    }

    /// Put a copy of this executable where the service will run it from.
    fn install_binary() -> Result<std::path::PathBuf> {
        let target = installed_exe_path()?;
        let source = std::env::current_exe().context("could not find this executable")?;

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }

        // Replacing a copy the service is still running fails, and should: the
        // caller stops the service first. Reported plainly rather than left as
        // a bare OS error, because "Access is denied" on an install is not
        // self-explanatory.
        std::fs::copy(&source, &target).with_context(|| {
            format!(
                "could not copy {} to {} — stop the service first if it is running",
                source.display(),
                target.display()
            )
        })?;
        Ok(target)
    }

    /// The command line the service manager should launch.
    ///
    /// Quoted because the path routinely contains spaces, and an unquoted path
    /// makes Windows launch the wrong file — the classic unquoted-service-path
    /// problem, which is a privilege-escalation hole and not merely a bug.
    fn service_command() -> Result<String> {
        Ok(format!("\"{}\" {RUN_FLAG}", installed_exe_path()?.display()))
    }

    fn open_manager(access: u32) -> Result<ScHandle> {
        // SAFETY: documented call; the handle is closed by ScHandle.
        let handle = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
            .context("could not open the service database — administrator rights are needed")?;
        Ok(ScHandle(handle))
    }

    fn open_service(manager: &ScHandle, access: u32) -> Result<Option<ScHandle>> {
        let name = wide(SERVICE_NAME);
        // SAFETY: documented call against a manager handle we hold.
        match unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) } {
            Ok(handle) => Ok(Some(ScHandle(handle))),
            Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
            Err(e) => Err(e).context("could not open the Tain service"),
        }
    }

    /// What state the service is in, without needing administrator rights.
    ///
    /// # The access mask matters more than it looks
    ///
    /// This asked for `SC_MANAGER_ALL_ACCESS` minus the create-service bit,
    /// which reads as "everything except the part that needs admin" and is
    /// wrong: `ALL_ACCESS` also carries `STANDARD_RIGHTS_REQUIRED`, and a
    /// normal user cannot obtain `WRITE_DAC`, `WRITE_OWNER` or `DELETE` on the
    /// service database either. So the open failed for every unelevated
    /// launch, the state came back `Unknown`, and the settings panel greyed out
    /// the Install button on machines where nothing was installed at all.
    ///
    /// Connect and enumerate are what reading a service's state actually
    /// needs, and both are granted to ordinary users.
    pub fn state() -> ServiceState {
        let Ok(manager) = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE) else {
            return ServiceState::Unknown;
        };
        let handle = match open_service(&manager, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) {
            Ok(Some(handle)) => handle,
            Ok(None) => return ServiceState::NotInstalled,
            Err(_) => return ServiceState::Unknown,
        };

        if !registered_against_this_exe(&handle) {
            return ServiceState::Mismatched;
        }

        let mut status = SERVICE_STATUS::default();
        // SAFETY: a service handle opened with query access.
        if unsafe { QueryServiceStatus(handle.0, &mut status) }.is_err() {
            return ServiceState::Unknown;
        }
        if status.dwCurrentState == SERVICE_RUNNING {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        }
    }

    /// Whether the registered binary path is the executable running now.
    fn registered_against_this_exe(handle: &ScHandle) -> bool {
        // The installed copy, not the running file. See `installed_exe_path`.
        let Ok(expected) = installed_exe_path().map(|p| p.display().to_string()) else {
            return true;
        };

        let mut needed = 0u32;
        // SAFETY: the documented two-call pattern — ask for the size, then
        // supply a buffer of it.
        unsafe {
            let _ = QueryServiceConfigW(handle.0, None, 0, &mut needed);
        }
        if needed == 0 {
            return true;
        }

        let mut buffer = vec![0u8; needed as usize];
        let config = buffer.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        // SAFETY: the buffer is the size the call just asked for.
        if unsafe { QueryServiceConfigW(handle.0, Some(config), needed, &mut needed) }.is_err() {
            return true;
        }

        // SAFETY: on success the struct is initialised and its path points
        // into the buffer that is still alive.
        let path = unsafe { (*config).lpBinaryPathName };
        if path.is_null() {
            return true;
        }
        // SAFETY: a null-terminated string owned by the buffer above.
        let registered = unsafe { PWSTR(path.0).to_string() }.unwrap_or_default();
        registered.to_lowercase().contains(&expected.to_lowercase())
    }

    /// Start the service if it is installed and stopped.
    ///
    /// Called when the application launches, so a machine that has the service
    /// registered has it running without anyone thinking about it. Registering
    /// it as auto-start means Windows normally does this at boot; this covers
    /// the case where it was stopped by hand or failed to start once.
    ///
    /// Quiet on failure. Starting a service is an administrator's right by
    /// default, so an ordinary launch will often not be permitted — that is
    /// expected, not an error, and the settings panel already says when the
    /// service is installed but not running.
    pub fn start_if_stopped() -> bool {
        let Ok(manager) = open_manager(SC_MANAGER_CONNECT) else { return false };
        let Ok(Some(service)) = open_service(&manager, SERVICE_START | SERVICE_QUERY_STATUS)
        else {
            return false;
        };

        let mut status = SERVICE_STATUS::default();
        // SAFETY: a handle opened with query access.
        if unsafe { QueryServiceStatus(service.0, &mut status) }.is_err()
            || status.dwCurrentState == SERVICE_RUNNING
        {
            return false;
        }

        // SAFETY: a handle opened with start access.
        match unsafe { StartServiceW(service.0, None) } {
            Ok(()) => {
                tracing::info!("Started the Tain service");
                true
            }
            Err(e) => {
                tracing::debug!("Could not start the Tain service ({e}) — needs administrator");
                false
            }
        }
    }

    /// Ask for elevation and register the service.
    ///
    /// Relaunches this executable with the install flag under the `runas` verb,
    /// which is what produces the consent prompt. Registering a service needs
    /// administrator rights and the app does not have them; there is no way to
    /// do this without the prompt, so it is better to expect it than to be
    /// surprised by it.
    pub fn request_install() -> Result<()> {
        elevate(INSTALL_FLAG)
    }

    /// The same, for removal.
    pub fn request_uninstall() -> Result<()> {
        elevate(UNINSTALL_FLAG)
    }

    fn elevate(flag: &str) -> Result<()> {
        let exe = wide(&exe_path()?);
        let verb = wide("runas");
        let args = wide(flag);

        // SAFETY: documented call. The returned HINSTANCE is a status code
        // rather than a handle, and values of 32 or less mean failure.
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(exe.as_ptr()),
                PCWSTR(args.as_ptr()),
                PCWSTR::null(),
                SW_HIDE,
            )
        };
        if result.0 as isize <= 32 {
            bail!("the elevation prompt was refused or failed");
        }
        Ok(())
    }

    /// Register the service. Must already be running as administrator.
    pub fn install() -> Result<()> {
        let manager = open_manager(SC_MANAGER_ALL_ACCESS)?;
        if open_service(&manager, SERVICE_ALL_ACCESS)?.is_some() {
            // Registered already — most likely against an older copy of the
            // executable, so replace it rather than reporting success on a
            // service that points somewhere else.
            uninstall()?;
        }

        // After the uninstall above, so the old service has released the copy
        // it was running before this replaces it.
        let installed = install_binary()?;
        tracing::info!("Installed the service binary at {}", installed.display());

        let name = wide(SERVICE_NAME);
        let display = wide(DISPLAY_NAME);
        let command = wide(&service_command()?);

        // SAFETY: documented call; every pointer is a null-terminated buffer
        // that outlives it.
        let service = unsafe {
            CreateServiceW(
                manager.0,
                PCWSTR(name.as_ptr()),
                PCWSTR(display.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(command.as_ptr()),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                // LocalSystem, which is the whole reason for the service: only
                // it may open the secure desktop.
                PCWSTR::null(),
                PCWSTR::null(),
            )
        }
        .context("could not register the Tain service")?;
        let service = ScHandle(service);

        // Someone who finds "TainHost" in the services list deserves to be told
        // what it is and that removing it is safe. Best-effort: a service
        // without a description still works, and failing the install over a
        // line of text would be absurd.
        let mut description = wide(DESCRIPTION);
        let text = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(description.as_mut_ptr()) };
        // SAFETY: the struct and its string outlive the call.
        unsafe {
            let _ = ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&text as *const _ as *const _),
            );
        }

        // Started now rather than at the next boot, so the feature the user
        // just switched on works immediately.
        // SAFETY: a service handle with start access.
        unsafe { StartServiceW(service.0, None) }.context("registered, but could not start")?;
        Ok(())
    }

    /// Stop and remove the service. Must already be running as administrator.
    pub fn uninstall() -> Result<()> {
        let manager = open_manager(SC_MANAGER_ALL_ACCESS)?;
        let Some(service) = open_service(&manager, SERVICE_ALL_ACCESS)? else {
            return Ok(());
        };

        let mut status = SERVICE_STATUS::default();
        // SAFETY: a stop request on a handle with control access. A service
        // that is already stopped reports an error, which is not a problem.
        unsafe {
            let _ = ControlService(service.0, SERVICE_CONTROL_STOP, &mut status);
        }

        // Deleting a running service leaves it marked for deletion until it
        // exits, and a reinstall in that window fails confusingly. Waiting a
        // moment for it to stop avoids the whole situation.
        for _ in 0..20 {
            // SAFETY: a handle with query access.
            if unsafe { QueryServiceStatus(service.0, &mut status) }.is_err()
                || status.dwCurrentState == SERVICE_STOPPED
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // SAFETY: a handle with delete access.
        unsafe { DeleteService(service.0) }.context("could not remove the Tain service")?;
        Ok(())
    }

    /// Set while the service should keep running.
    static RUNNING: AtomicBool = AtomicBool::new(true);
    /// The status handle, needed by the control callback.
    static STATUS_HANDLE: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);


    /// The desktops worth having an agent on.
    ///
    /// `Default` is the ordinary one. `Winlogon` is where the secure desktop
    /// lives — UAC consent, the lock screen, Ctrl+Alt+Del — and is the whole
    /// reason a service exists here, because nothing running as the user may
    /// touch it.
    const DESKTOPS: [&str; 2] = ["Default", "Winlogon"];

    /// Keeps one agent alive on each desktop of the console session.
    ///
    /// # Why not one agent that follows the input
    ///
    /// That was the first attempt and it could not work. A service is in
    /// session zero's window station, and `OpenInputDesktop` answers for the
    /// caller's own station, so the service has no way to see which desktop the
    /// person at the keyboard is on. Worse, it does not fail loudly: it returns
    /// nothing, so the supervisor asked an unanswerable question twice a second
    /// and did nothing at all, silently, for a whole test.
    ///
    /// A service *can* start a process on a named desktop inside another
    /// session, so that is what this does. Both agents run; each decides for
    /// itself whether it currently owns input. Only one ever does.
    struct AgentSupervisor {
        /// One entry per desktop in [`DESKTOPS`], once started.
        running: Vec<(&'static str, HANDLE)>,
        /// The session the current agents belong to, so a sign-out that changes
        /// it replaces them rather than leaving them behind.
        session: Option<u32>,
        /// Desktops already complained about, so a failure that repeats every
        /// half second is logged once.
        complained: Vec<&'static str>,
    }

    impl AgentSupervisor {
        fn new() -> Self {
            Self { running: Vec::new(), session: None, complained: Vec::new() }
        }

        /// Bring the agents into line with the console session. On a timer.
        fn poll(&mut self) {
            let session = console_session();

            // A different session means different agents. Sign-out and back in,
            // or switching users, and the old ones belong to a session that is
            // gone.
            if self.session != session {
                if self.session.is_some() {
                    tracing::info!("Console session changed to {session:?} — replacing agents");
                }
                self.stop_all();
                self.session = session;
            }

            let Some(session) = session else { return };

            // Anything that exited is no longer running, whatever it was for.
            self.running.retain(|(desktop, handle)| {
                let alive = process_is_alive(*handle);
                if !alive {
                    tracing::info!("Agent on {desktop} exited");
                    // SAFETY: created by this supervisor, closed once, here.
                    unsafe {
                        let _ = CloseHandle(*handle);
                    }
                }
                alive
            });

            for desktop in DESKTOPS {
                if self.running.iter().any(|(on, _)| *on == desktop) {
                    continue;
                }
                match spawn_agent(session, desktop) {
                    Ok(handle) => {
                        tracing::info!("Agent started on desktop {desktop} in session {session}");
                        self.running.push((desktop, handle));
                        self.complained.retain(|d| *d != desktop);
                    }
                    Err(e) => {
                        // Once per desktop rather than twice a second. During a
                        // session change there is a window where the token is
                        // not available yet, and it resolves on its own.
                        if !self.complained.contains(&desktop) {
                            tracing::warn!("Could not start an agent on {desktop}: {e:#}");
                            self.complained.push(desktop);
                        }
                    }
                }
            }
        }

        fn stop_all(&mut self) {
            for (desktop, handle) in std::mem::take(&mut self.running) {
                if process_is_alive(handle) {
                    // SAFETY: a process handle this supervisor created.
                    unsafe {
                        let _ = TerminateProcess(handle, 0);
                    }
                    tracing::debug!("Agent on {desktop} stopped");
                }
                // SAFETY: created by this supervisor, closed once.
                unsafe {
                    let _ = CloseHandle(handle);
                }
            }
            self.complained.clear();
        }
    }

    impl Drop for AgentSupervisor {
        fn drop(&mut self) {
            self.stop_all();
        }
    }

    /// The session signed in at the physical console, if there is one.
    fn console_session() -> Option<u32> {
        use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

        // SAFETY: a documented read of global session state.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        // `0xFFFFFFFF` means no session is attached — during a transition, or
        // on a machine nobody is signed in to.
        (session != u32::MAX).then_some(session)
    }

    /// Whether a process handle still refers to something running.
    fn process_is_alive(handle: HANDLE) -> bool {
        use windows::Win32::System::Threading::GetExitCodeProcess;

        /// What `GetExitCodeProcess` reports for a process that has not exited.
        /// Named here because `STILL_ACTIVE` lives among the NT status codes
        /// rather than with the process API.
        const STILL_RUNNING: u32 = 259;

        let mut code = 0u32;
        // SAFETY: a process handle this module owns.
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        ok && code == STILL_RUNNING
    }

    /// Start an agent on `desktop` in `session`, as the right identity for it.
    ///
    /// # Two identities, deliberately
    ///
    /// On the ordinary desktop the agent runs as the signed-in user: it is
    /// their session being captured, and their profile and audio endpoints are
    /// what it needs. On the secure desktop no user token is permitted at all —
    /// that separation is the entire point of UAC — so the agent there runs as
    /// SYSTEM, which the service already is.
    fn spawn_agent(session: u32, desktop: &str) -> Result<HANDLE> {
        let token = if desktop.eq_ignore_ascii_case("Default") {
            console_user_token(session)?
        } else {
            system_token_for_session(session)?
        };
        start_with_token(&token, desktop)
    }

    /// A primary token for the user signed in at the console.
    fn console_user_token(session: u32) -> Result<OwnedHandle> {
        use windows::Win32::Security::{
            DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
        };
        use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;

        // SAFETY: called as SYSTEM, which is what this privilege requires; the
        // handle is owned from here on.
        let mut user = HANDLE::default();
        unsafe { WTSQueryUserToken(session, &mut user) }
            .context("no user is signed in at the console yet")?;
        let user = OwnedHandle(user);

        // `CreateProcessAsUser` will not take the impersonation token
        // `WTSQueryUserToken` hands out.
        let mut primary = HANDLE::default();
        // SAFETY: duplicating a token this function owns.
        unsafe {
            DuplicateTokenEx(
                user.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        }
        .context("could not duplicate the console user's token")?;
        Ok(OwnedHandle(primary))
    }

    /// A copy of this service's own SYSTEM token, moved into `session`.
    ///
    /// # Why the session has to be set explicitly
    ///
    /// A process created from a service inherits session zero, and a process in
    /// session zero cannot see the interactive desktops at all — naming
    /// `WinSta0\Winlogon` from there resolves to session zero's, not the one
    /// the consent prompt is on. Setting `TokenSessionId` moves the new process
    /// into the console session while leaving it as SYSTEM, which is the only
    /// combination the secure desktop accepts. It needs `SE_TCB_NAME`, which
    /// Local System has and nothing else does.
    fn system_token_for_session(session: u32) -> Result<OwnedHandle> {
        use windows::Win32::Security::{
            DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary,
            TokenSessionId, TOKEN_ALL_ACCESS,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut own = HANDLE::default();
        // SAFETY: opening our own process token; owned from here on.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) }
            .context("could not open the service's own token")?;
        let own = OwnedHandle(own);

        let mut primary = HANDLE::default();
        // SAFETY: duplicating a token this function owns.
        unsafe {
            DuplicateTokenEx(
                own.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        }
        .context("could not duplicate the service's token")?;
        let primary = OwnedHandle(primary);

        // SAFETY: a `u32` for `TokenSessionId`, which is what it documents.
        unsafe {
            SetTokenInformation(
                primary.0,
                TokenSessionId,
                &session as *const u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            )
        }
        .context("could not move the service's token into the console session")?;
        Ok(primary)
    }

    /// Start the agent with `token`, on `desktop`.
    fn start_with_token(token: &OwnedHandle, desktop: &str) -> Result<HANDLE> {
        use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
        use windows::Win32::System::Threading::{
            CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
            PROCESS_INFORMATION, STARTUPINFOW,
        };

        // The environment that goes with the token, so the agent reads the
        // right profile. Not fatal if it cannot be built.
        let mut environment = std::ptr::null_mut();
        // SAFETY: documented; destroyed below if it was created.
        let have_env = unsafe { CreateEnvironmentBlock(&mut environment, token.0, false) }.is_ok();

        let mut command = wide(&format!(
            "\"{}\" {} {} \"{}\"",
            exe_path()?,
            crate::agent::AGENT_FLAG,
            crate::agent::DESKTOP_ARG,
            desktop
        ));
        let mut desktop_path = wide(&format!("WinSta0\\{desktop}"));
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_path.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();

        // SAFETY: every pointer outlives the call, and the handles it returns
        // are taken ownership of immediately below.
        let started = unsafe {
            CreateProcessAsUserW(
                token.0,
                None,
                PWSTR(command.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                if have_env { Some(environment) } else { None },
                None,
                &startup,
                &mut info,
            )
        };

        if have_env {
            // SAFETY: created immediately above.
            unsafe {
                let _ = DestroyEnvironmentBlock(environment);
            }
        }
        started.context("CreateProcessAsUser refused to start the agent")?;

        // The thread handle is of no use to anyone here.
        // SAFETY: returned by the call above, closed once.
        unsafe {
            let _ = CloseHandle(info.hThread);
        }
        Ok(info.hProcess)
    }

    /// Closes a handle however the surrounding code exits.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: owned by this type, closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// Whether the application has said it is hosting.
    ///
    /// Two hosts on one identity is worse than none: a guest reaches whichever
    /// answers first, and which that is changes between attempts. So this
    /// yields to the application whenever it is up.
    static APPLICATION_HOSTING: AtomicBool = AtomicBool::new(false);

    /// Take the control channel and keep the service's copy of the identity.
    ///
    /// Runs on its own thread for the life of the service: accepting a
    /// connection blocks, and the supervisor loop has agents to watch.
    fn serve_control() {
        use tain_core::service_control::{serve, ControlMessage};

        let secrets = crate::gui::secrets::HostSecrets::os();
        let served = serve(&RUNNING, |message| match message {
            ControlMessage::AdoptIdentity { secret_key, codes } => {
                if !secrets.set_identity(&secret_key) {
                    tracing::warn!("Could not store the host identity — unattended access \
                                    will not survive a restart");
                }
                // Parsed rather than trusted. A code is base58check, so a
                // damaged one fails here instead of being adopted as a
                // passcode nobody can produce.
                let mut kept = 0;
                for code in &codes {
                    match tain_core::ConnectionCode::parse(code) {
                        Ok(parsed) => {
                            if secrets.set_code(parsed.passcode_type, code) {
                                kept += 1;
                            }
                        }
                        Err(e) => tracing::warn!("The application sent an unreadable code: {e}"),
                    }
                }
                tracing::info!(
                    "Adopted the host identity and {kept} of {} codes — this machine can \
                     answer with nobody signed in",
                    codes.len()
                );
            }
            ControlMessage::ForgetIdentity => {
                secrets.forget_code(tain_core::PasscodeType::Master);
                secrets.forget_code(tain_core::PasscodeType::Guest);
                tracing::info!("Forgot the host identity — unattended access is off");
            }
            ControlMessage::ApplicationHosting(hosting) => {
                let was = APPLICATION_HOSTING.swap(hosting, Ordering::Relaxed);
                if was != hosting {
                    tracing::info!(
                        "The application is {} hosting",
                        if hosting { "now" } else { "no longer" }
                    );
                }
            }
        });
        if let Err(e) = served {
            tracing::warn!("The service control channel stopped: {e:#}");
        }
    }

    /// Hosts on this machine's behalf while nobody is signed in.
    struct UnattendedHost {
        /// The running host, and the flag that stops it.
        running: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
    }

    impl UnattendedHost {
        fn new() -> Self {
            Self { running: None }
        }

        /// Start or stop hosting to match what the machine currently needs.
        ///
        /// # When the service should be the host
        ///
        /// Only when there is nobody to be one instead. A signed-in user has
        /// the application, which owns the same identity and the same codes —
        /// and answering alongside it would mean a guest reaching whichever
        /// bound first.
        ///
        /// `WTSQueryUserToken` failing *is* the test for "nobody is signed in".
        /// The console session exists at the sign-in screen; what it lacks is a
        /// user, and asking for that user's token is how Windows says so.
        fn poll(&mut self) {
            let wanted = !APPLICATION_HOSTING.load(Ordering::Relaxed)
                && console_user_token(console_session().unwrap_or(u32::MAX)).is_err();

            // A host that stopped on its own — a bind that failed, a network
            // that went away — is restarted by the next pass.
            if let Some((_, handle)) = &self.running
                && handle.is_finished()
            {
                self.stop();
            }

            match (wanted, self.running.is_some()) {
                (true, false) => self.start(),
                (false, true) => {
                    tracing::info!("Somebody is signed in — handing hosting back");
                    self.stop();
                }
                _ => {}
            }
        }

        fn start(&mut self) {
            let secrets = crate::gui::secrets::HostSecrets::os();
            let Some(identity) = secrets.identity() else {
                // Nothing adopted, so unattended access is off or was never
                // configured. Not a failure and not worth a line every pass.
                return;
            };

            tracing::info!("Nobody is signed in — hosting on this machine's behalf");
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let spawned = std::thread::Builder::new()
                .name("tain-unattended-host".into())
                .spawn(move || host_until_stopped(identity, &flag));

            match spawned {
                Ok(handle) => self.running = Some((stop, handle)),
                Err(e) => tracing::warn!("Could not start the unattended host: {e}"),
            }
        }

        fn stop(&mut self) {
            let Some((stop, handle)) = self.running.take() else { return };
            stop.store(true, Ordering::Relaxed);
            // Not joined. The host is inside its own runtime and may be waiting
            // on a socket; the flag ends it at the next tick, and blocking the
            // service's own loop on that would delay a stop request.
            drop(handle);
        }
    }

    impl Drop for UnattendedHost {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Run a host with the adopted identity until told to stop.
    ///
    /// On its own runtime rather than the application's: the service main runs
    /// on a thread the control manager created, with no runtime of its own, and
    /// borrowing one across that boundary is more machinery than a runtime
    /// costs.
    fn host_until_stopped(identity: [u8; 32], stop: &AtomicBool) {
        // Session zero has no desktop, so every frame has to come from the
        // agent on the secure desktop — which, with nobody signed in, is the
        // sign-in screen and the only thing there is to show.
        tain_core::capture_follow::use_agent_only();

        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                tracing::warn!("Could not start a runtime for the unattended host: {e}");
                return;
            }
        };

        runtime.block_on(async move {
            let server = match tain_core::HostServer::bind_with_identity(Some(identity)).await {
                Ok(server) => Arc::new(server),
                Err(e) => {
                    tracing::warn!("The unattended host could not bind: {e:#}");
                    return;
                }
            };

            // Codes are restored rather than reissued. A new one would name the
            // same endpoint and still be a code nobody has been given.
            let secrets = crate::gui::secrets::HostSecrets::os();
            let endpoint_id = server.endpoint_id_bytes();
            for typ in [tain_core::PasscodeType::Master, tain_core::PasscodeType::Guest] {
                let Some(saved) = secrets.code(typ) else { continue };
                match tain_core::ConnectionCode::parse(&saved) {
                    Ok(parsed) if parsed.endpoint_id == endpoint_id => {
                        server.adopt_passcode(typ, parsed.secret).await;
                    }
                    // A code for a different endpoint is one the application
                    // regenerated without telling us. Refusing it is right:
                    // adopting it would answer to a secret whose code names
                    // somewhere else.
                    Ok(_) => tracing::warn!(
                        "The stored {} code is for a different endpoint — ignoring it",
                        typ.prefix()
                    ),
                    Err(e) => tracing::warn!("The stored {} code is unreadable: {e}", typ.prefix()),
                }
            }

            // No approval channel, deliberately. Nobody is signed in to answer
            // one, and `await_approval` refuses a guest that has no channel —
            // so a MASTER code connects and a GUEST code is declined rather
            // than hanging until it times out or, worse, being let through.
            tracing::info!("Unattended host listening — master codes only");

            let serving = server.clone().run();
            tokio::pin!(serving);
            loop {
                tokio::select! {
                    result = &mut serving => {
                        if let Err(e) = result {
                            tracing::warn!("The unattended host stopped: {e:#}");
                        }
                        return;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        if stop.load(Ordering::Relaxed) {
                            tracing::info!("The unattended host was asked to stop");
                            return;
                        }
                    }
                }
            }
        });
    }

    fn report(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE) {
        let Ok(handle) = STATUS_HANDLE.lock() else { return };
        let Some(handle) = *handle else { return };

        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP } else { 0 },
            ..Default::default()
        };
        // SAFETY: a status handle from RegisterServiceCtrlHandlerW.
        unsafe {
            let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut _), &status);
        }
    }

    unsafe extern "system" fn control_handler(control: u32) {
        if control == SERVICE_CONTROL_STOP {
            report(SERVICE_STOP_PENDING);
            RUNNING.store(false, Ordering::Relaxed);
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
        let name = wide(SERVICE_NAME);
        // SAFETY: called from the service main, as documented.
        let handle = unsafe {
            RegisterServiceCtrlHandlerW(PCWSTR(name.as_ptr()), Some(control_handler))
        };
        let Ok(handle) = handle else { return };
        if let Ok(mut slot) = STATUS_HANDLE.lock() {
            *slot = Some(handle.0 as isize);
        }

        report(SERVICE_RUNNING);
        tracing::info!("Tain service started");

        // The application talks to this on its own thread, because accepting a
        // connection blocks and this loop has work to do.
        std::thread::Builder::new()
            .name("tain-service-control".into())
            .spawn(serve_control)
            .ok();

        // One agent per desktop of the console session. See `AgentSupervisor`
        // for why it is not one that follows the input around.
        let mut agents = AgentSupervisor::new();
        let mut unattended = UnattendedHost::new();
        while RUNNING.load(Ordering::Relaxed) {
            agents.poll();
            unattended.poll();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        drop(unattended);
        drop(agents);

        tracing::info!("Tain service stopping");
        report(SERVICE_STOPPED);
    }

    /// Hand this process to the service control manager.
    ///
    /// Only ever reached via [`RUN_FLAG`], which the service manager supplies.
    /// Run by hand it fails immediately, which is correct — a service main
    /// outside a service has nothing to talk to.
    pub fn run() -> Result<()> {
        let mut name = wide(SERVICE_NAME);
        let table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: PWSTR(name.as_mut_ptr()),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW::default(),
        ];

        // SAFETY: the table is null-terminated by the default entry and lives
        // until this call returns, which it does only once the service ends.
        unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
            .context("this process was not started by the service manager")?;
        Ok(())
    }

    /// Silences an unused-import warning on the handle type in stub builds.
    #[allow(dead_code)]
    fn _unused(_: HANDLE) {}
}
