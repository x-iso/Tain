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

    /// The command line the service manager should launch.
    ///
    /// Quoted because the path routinely contains spaces, and an unquoted path
    /// makes Windows launch the wrong file — the classic unquoted-service-path
    /// problem, which is a privilege-escalation hole and not merely a bug.
    fn service_command() -> Result<String> {
        Ok(format!("\"{}\" {RUN_FLAG}", exe_path()?))
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
        let Ok(expected) = exe_path() else { return true };

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


    /// Keeps exactly one agent alive, on whichever desktop is receiving input.
    ///
    /// # Why the service has to be the one doing this
    ///
    /// A process is bound to the desktop it was created on and cannot move.
    /// Starting one on a *different* desktop needs `CreateProcessAsUser` with
    /// `lpDesktop` set, and starting anything on the secure desktop needs to be
    /// SYSTEM. Both are true of a service and of nothing the user runs.
    ///
    /// So the loop is: look at which desktop has input, compare it with where
    /// the agent is, and if they differ replace the agent. The agent exits by
    /// itself when it notices the same thing, so the usual case is that it has
    /// already gone by the time this looks.
    struct AgentSupervisor {
        /// The running agent: its process handle and the desktop it is on.
        current: Option<(HANDLE, String)>,
        /// The last desktop this failed to start an agent on, so a failure
        /// that repeats every half second is logged once.
        complained_about: Option<String>,
    }

    impl AgentSupervisor {
        fn new() -> Self {
            Self { current: None, complained_about: None }
        }

        /// Bring the agent into line with the input desktop. Called on a timer.
        fn poll(&mut self) {
            // An agent that has exited is not an agent, whatever it was for.
            if let Some((handle, _)) = self.current
                && !process_is_alive(handle)
            {
                self.close_current();
            }

            let Some(desktop) = crate::agent::input_desktop_name_for_service() else {
                // No input desktop readable at all. Nothing to do but wait;
                // this happens between a session ending and the next starting.
                return;
            };

            match &self.current {
                Some((_, on)) if on.eq_ignore_ascii_case(&desktop) => return,
                Some((_, on)) => {
                    tracing::info!("Input moved from {on} to {desktop} — replacing the agent");
                    self.close_current();
                }
                None => {}
            }

            match spawn_agent(&desktop) {
                Ok(handle) => {
                    tracing::info!("Agent started on desktop {desktop}");
                    self.current = Some((handle, desktop));
                    self.complained_about = None;
                }
                Err(e) => {
                    // Logged once per desktop rather than twice a second. A
                    // desktop that cannot be reached now often can be shortly —
                    // during a session change there is a window where the token
                    // is not available yet — so this keeps trying quietly.
                    if self.complained_about.as_deref() != Some(desktop.as_str()) {
                        tracing::warn!("Could not start an agent on desktop {desktop}: {e:#}");
                        self.complained_about = Some(desktop);
                    }
                }
            }
        }

        /// End the agent, if there is one.
        fn close_current(&mut self) {
            let Some((handle, on)) = self.current.take() else { return };
            if process_is_alive(handle) {
                // SAFETY: a process handle this supervisor created.
                unsafe {
                    let _ = TerminateProcess(handle, 0);
                }
                tracing::debug!("Agent on {on} was still running and has been stopped");
            }
            // SAFETY: created by `CreateProcessAsUserW`, closed once.
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }

    impl Drop for AgentSupervisor {
        fn drop(&mut self) {
            self.close_current();
        }
    }

    /// Whether a process handle still refers to something running.
    fn process_is_alive(handle: HANDLE) -> bool {
        use windows::Win32::System::Threading::GetExitCodeProcess;

        /// What `GetExitCodeProcess` reports for a process that has not
        /// exited. Named here because `STILL_ACTIVE` lives among the NT status
        /// codes rather than with the process API.
        const STILL_RUNNING: u32 = 259;

        let mut code = 0u32;
        // SAFETY: a process handle this module owns.
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        ok && code == STILL_RUNNING
    }

    /// Start an agent on `desktop`, as the right identity for that desktop.
    ///
    /// # Two identities, deliberately
    ///
    /// On the ordinary desktop the agent runs as the logged-in user: it is
    /// their session being captured, and their profile, audio endpoints and
    /// per-user settings are what it needs. On the secure desktop no user token
    /// is permitted at all — that separation is the entire point of UAC — so
    /// the agent there runs as SYSTEM, which the service already is.
    fn spawn_agent(desktop: &str) -> Result<HANDLE> {
        let secure = !desktop.eq_ignore_ascii_case("Default");
        if secure {
            spawn_as_system(desktop)
        } else {
            spawn_as_console_user(desktop)
        }
    }

    /// The command line for an agent on `desktop`.
    fn agent_command(desktop: &str) -> Result<Vec<u16>> {
        Ok(wide(&format!(
            "\"{}\" {} {} \"{}\"",
            exe_path()?,
            crate::agent::AGENT_FLAG,
            crate::agent::DESKTOP_ARG,
            desktop
        )))
    }

    /// `WinSta0\<desktop>`, which is what `STARTUPINFO` wants.
    fn desktop_path(desktop: &str) -> Vec<u16> {
        wide(&format!("WinSta0\\{desktop}"))
    }

    /// Start the agent as the user signed in at the physical console.
    fn spawn_as_console_user(desktop: &str) -> Result<HANDLE> {
        use windows::Win32::Security::{
            DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
        };
        use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
        use windows::Win32::System::RemoteDesktop::{
            WTSGetActiveConsoleSessionId, WTSQueryUserToken,
        };
        use windows::Win32::System::Threading::{
            CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
        };

        // SAFETY: a documented read of global session state.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        anyhow::ensure!(session != u32::MAX, "there is no console session to run an agent in");

        // SAFETY: called as SYSTEM, which is what this privilege requires; the
        // token is closed below.
        let mut user_token = HANDLE::default();
        unsafe { WTSQueryUserToken(session, &mut user_token) }
            .context("no user is signed in at the console yet")?;
        let user_token = OwnedHandle(user_token);

        // A primary token, because `CreateProcessAsUser` will not take the
        // impersonation token `WTSQueryUserToken` hands out.
        let mut primary = HANDLE::default();
        // SAFETY: duplicating a token this function owns.
        unsafe {
            DuplicateTokenEx(
                user_token.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        }
        .context("could not duplicate the console user's token")?;
        let primary = OwnedHandle(primary);

        // The user's environment, so the agent sees their profile rather than
        // the service's. Not fatal if it cannot be built.
        let mut environment = std::ptr::null_mut();
        // SAFETY: documented; destroyed below if it was created.
        let have_env =
            unsafe { CreateEnvironmentBlock(&mut environment, primary.0, false) }.is_ok();

        let mut command = agent_command(desktop)?;
        let mut desktop_name = desktop_path(desktop);
        let mut startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_name.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();

        // SAFETY: every pointer outlives the call, and the handles the call
        // returns are taken ownership of immediately below.
        let started = unsafe {
            CreateProcessAsUserW(
                primary.0,
                None,
                PWSTR(command.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT,
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
        let _ = &mut startup;
        Ok(info.hProcess)
    }

    /// Start the agent as SYSTEM, which is the only identity the secure desktop
    /// accepts.
    fn spawn_as_system(desktop: &str) -> Result<HANDLE> {
        use windows::Win32::System::Threading::{
            CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
        };

        let mut command = agent_command(desktop)?;
        let mut desktop_name = desktop_path(desktop);
        let mut startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_name.as_mut_ptr()),
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();

        // SAFETY: every pointer outlives the call; the returned handles are
        // taken ownership of below.
        unsafe {
            CreateProcessW(
                None,
                PWSTR(command.as_mut_ptr()),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(0),
                None,
                None,
                &startup,
                &mut info,
            )
        }
        .context("CreateProcess refused to start the agent on the secure desktop")?;

        // SAFETY: returned by the call above, closed once.
        unsafe {
            let _ = CloseHandle(info.hThread);
        }
        let _ = &mut startup;
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

        // One agent, on whichever desktop is receiving input, replaced when
        // that changes. See `AgentSupervisor` for why the service has to be the
        // one doing it.
        let mut agents = AgentSupervisor::new();
        while RUNNING.load(Ordering::Relaxed) {
            agents.poll();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
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
