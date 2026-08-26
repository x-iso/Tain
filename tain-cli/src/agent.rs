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
//! hence this.
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
//! # One agent per desktop, not one that follows
//!
//! The obvious arrangement — have the service watch which desktop is receiving
//! input and keep a single agent on it — was the first version, and it cannot
//! work. A service cannot do that: `OpenInputDesktop`
//! answers for the caller's own window station, and a service is in session
//! zero's, which has no relation to the `WinSta0` the person at the keyboard is
//! using. The call did not fail loudly — it returned nothing, so the supervisor
//! asked an unanswerable question twice a second and silently did nothing, for
//! an entire test.
//!
//! What a service *can* do is put a process on a named desktop inside another
//! session. So there is now one agent per desktop that matters, both alive at
//! once, and each works out for itself whether it currently owns the input.
//! Only one ever does, and the answer costs two handle operations.
//!
//! # Capture was proved before it was plumbed
//!
//! Both were checked before anything was built on them, which on this feature
//! has repeatedly been the difference between an afternoon and a week. Placing
//! the process: both agents start in session one, and a consent prompt hands
//! over between them in about fifty milliseconds. Capturing from there:
//! 2560x1600, first attempt, 99.9% of pixels not black — a real consent prompt
//! over a real dimmed desktop.
//!
//! The second of those was the one in doubt. Desktop Duplication has its own
//! rules about which desktop and which session may open an output, and being
//! SYSTEM does not automatically settle them.
//!
//! # The probe that answered it also broke what came next
//!
//! It ran once, the first time the agent held the input, and created its own
//! capturer to do it. That was harmless while nothing else captured. The moment
//! serving arrived it was not: both attach to the same shared capture, they
//! started within microseconds of each other, and the probe took the first
//! frame.
//!
//! Which was the only frame there was going to be. Desktop Duplication reports
//! *changes*, and a consent prompt does not move — so after the probe consumed
//! the opening image the serving loop saw nothing for the rest of the prompt,
//! sent nothing, and the guest kept showing the last frame from the ordinary
//! desktop. A freeze, with nothing failing anywhere to say so.
//!
//! The probe is gone. What it measured is measured on the frames actually
//! served, where it costs one pass over the first frame and cannot take
//! anything away from anybody.

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
pub use platform::run;

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::Context;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// How often the agent checks that its desktop is still the live one.
    ///
    /// Frequent enough that a switch to the secure desktop is handed over in a
    /// fraction of a second, cheap enough to be free: two handle operations and
    /// a string compare.
    const WATCH: std::time::Duration = std::time::Duration::from_millis(300);

    static RUNNING: AtomicBool = AtomicBool::new(true);

    /// Stay on this desktop, and keep track of whether it currently has input.
    ///
    /// Does not exit when input moves elsewhere. An agent is cheap and a
    /// desktop switch is common — locking the screen, a consent prompt, the
    /// task switcher — so tearing one down and building another on every switch
    /// would put a process launch in the path of something that has to be
    /// instant. Both agents stay; only one is ever the live one.
    pub fn run(desktop: Option<String>) -> Result<()> {
        let started_on = desktop.or_else(current_desktop_name).unwrap_or_default();
        tracing::info!(
            "Tain agent running on desktop {started_on} (session {}, {})",
            session_id(),
            if is_system() { "as SYSTEM" } else { "as the logged-in user" },
        );

        // Logged on change rather than on a timer. The transition is the event
        // worth seeing — a UAC prompt appearing is one line in the Winlogon
        // agent's log and one in the other's — and a line every 300ms would
        // bury it.
        let mut had_input: Option<bool> = None;
        // The pipe waits on its own thread, because accepting a connection
        // blocks and this loop has a desktop to watch.
        let serving = {
            let desktop = started_on.clone();
            std::thread::Builder::new()
                .name("tain-agent-pipe".into())
                .spawn(move || serve_frames(desktop))
                .context("could not start the agent's pipe thread")?
        };
        while RUNNING.load(Ordering::Relaxed) {
            // `None` means the input desktop cannot be opened from here, which
            // is what the ordinary desktop sees while the secure one is up. It
            // is an answer, not a failure.
            let live = input_desktop_name();
            let mine = live.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(&started_on));

            if had_input != Some(mine) {
                match (had_input.is_some(), mine) {
                    (_, true) => tracing::info!("Desktop {started_on} now has the input"),
                    (true, false) => tracing::info!(
                        "Desktop {started_on} no longer has the input (it is on {})",
                        live.as_deref().unwrap_or("a desktop this agent cannot open"),
                    ),
                    (false, false) => tracing::info!(
                        "Desktop {started_on} does not have the input yet (it is on {})",
                        live.as_deref().unwrap_or("a desktop this agent cannot open"),
                    ),
                }
                had_input = Some(mine);
            }

            std::thread::sleep(WATCH);
        }
        let _ = serving.join();
        Ok(())
    }

    /// Serve this desktop's frames for as long as a host wants them.
    ///
    /// # Capture starts and stops with the host
    ///
    /// A duplication is a GPU resource, and an agent on the secure desktop is
    /// alive from boot to shutdown while the desktop it watches is up for a few
    /// seconds a day. So nothing is captured until a host connects, and the
    /// capturer is dropped the moment it goes.
    ///
    /// # Only while this desktop has the input
    ///
    /// Duplicating a desktop nobody is looking at produces nothing anyway —
    /// Desktop Duplication reports no new frame when nothing changes — but it
    /// would still hold the resource. Checking is two handle operations, and it
    /// means the agent that is not in use costs precisely nothing.
    fn serve_frames(desktop: String) {
        use tain_core::agent_pipe::{serve_forever, PipeFrame};
        use tain_core::screen_capture::{create_capturer, CaptureConfig};

        let served = serve_forever(&desktop, &RUNNING, |writer| {
            let mut capturer = create_capturer(CaptureConfig::default())
                .context("could not start capturing for the host")?;
            // Injection belongs to this process because `SendInput` reaches the
            // desktop of the thread that calls it, and this is the only process
            // on that desktop. It is also why the host cannot simply do it
            // itself while a consent prompt is up.
            let injector = tain_core::input_inject::create_injector();
            tracing::info!("Serving {desktop}: capture started for a connected host");

            // Measured on the way past, on the first frame only. A duplication
            // that starts, returns a rectangle of zeroes and reports success
            // looks exactly like a working one from a return code, and on the
            // secure desktop that is a real possibility rather than a
            // hypothetical: one machine reported 99.9% of pixels lit and
            // another 0.0%, from the same code.
            let mut sent = 0u64;

            while RUNNING.load(Ordering::Relaxed) {
                // Drained every pass, including the ones where nothing is
                // captured. A guest moving the pointer over a still prompt
                // produces input and no frames, and holding those until the
                // picture changed would make the pointer lag behind the hand.
                for event in writer.take_input() {
                    if let Err(e) = tain_core::network::inject_input(&*injector, &event) {
                        tracing::debug!("Serving {desktop}: could not inject: {e:#}");
                    }
                }

                // The host asked for this desktop, so it wants it whether or
                // not it currently has the input — but there is nothing to
                // capture until it does, and asking is cheaper than duplicating.
                if !owns_input(&desktop) {
                    std::thread::sleep(WATCH);
                    continue;
                }

                match capturer.capture_next() {
                    // Desktop Duplication says nothing changed, which on a
                    // consent prompt is most of the time. Not an error, and not
                    // worth a frame.
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(16)),
                    Ok(Some(frame)) => {
                        if sent == 0 {
                            let lit = frame
                                .data
                                .chunks_exact(4)
                                .filter(|p| p[0] | p[1] | p[2] != 0)
                                .count();
                            let total = (frame.data.len() / 4).max(1);
                            tracing::info!(
                                "Serving {desktop}: first frame {}x{}, {:.1}% of pixels not \
                                 black",
                                frame.width,
                                frame.height,
                                lit as f32 * 100.0 / total as f32,
                            );
                        }
                        writer.send(&PipeFrame {
                            width: frame.width,
                            height: frame.height,
                            format: frame.format as u8,
                            data: frame.data.to_vec(),
                        })?;
                        sent += 1;
                        // Every hundred, so a stream that is running says so
                        // without a line per frame.
                        if sent.is_multiple_of(100) {
                            tracing::debug!("Serving {desktop}: {sent} frames sent");
                        }
                    }
                    Err(e) => {
                        // A desktop switch invalidates a duplication, and the
                        // next connection will make a fresh one. Ending the
                        // session is how that is asked for.
                        anyhow::bail!("capture failed: {e:#}");
                    }
                }
            }
            Ok(())
        });

        if let Err(e) = served {
            tracing::warn!("Serving {desktop}: the pipe could not be served: {e:#}");
        }
    }

    /// Whether this desktop currently has the input.
    fn owns_input(desktop: &str) -> bool {
        input_desktop_name().is_some_and(|live| live.eq_ignore_ascii_case(desktop))
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
