//! A capturer that keeps producing frames when the desktop changes underneath
//! it.
//!
//! # The gap this closes
//!
//! A process can only capture the desktop it was created on. When Windows
//! raises a consent prompt it moves the input to the secure desktop, and the
//! host — running as the signed-in user on `Default` — can see nothing from
//! then on. What the guest gets is one dimmed frame and then a still picture
//! with a frozen pointer, which is exactly how this was reported.
//!
//! # Why it belongs here and not further up
//!
//! The obvious place to handle it is wherever the frame loop lives, which would
//! mean the loop, the encoder, the health reporting and the network path all
//! learning that a frame can come from somewhere else. None of them benefit from
//! knowing.
//!
//! A capturer that quietly changes where its frames come from is a smaller
//! idea, and everything above it keeps working unchanged: the same frames, the
//! same format, the same size. The switch is invisible past this file.
//!
//! # Input goes back the same way
//!
//! Seeing a consent prompt is half of it. `SendInput` from a process on the
//! ordinary desktop cannot reach the secure one either, so while the agent is
//! supplying frames it must also be the one injecting — otherwise the guest
//! watches a prompt it cannot answer, which is barely better than not seeing
//! it.
//!
//! The connection is shared rather than opened twice. The pipe accepts one
//! client, and more importantly the two directions have to agree about *when*
//! they are in use: frames arriving from the agent and clicks going anywhere
//! else would put the pointer on the wrong desktop. One link, opened by
//! whichever side needs it first, is the only arrangement where that cannot
//! come apart.
//!
//! # It only reaches for the agent when it has to
//!
//! The local backend is used whenever this desktop has the input, which is
//! almost always. The pipe is opened only once the input has gone somewhere
//! this process cannot see, and closed as soon as it comes back — so the normal
//! path is untouched, and the agent captures nothing until somebody is actually
//! looking at a consent prompt.

use anyhow::Result;

use super::screen_capture::{PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame};

/// The desktop the secure agent serves.
///
/// Only one is followed. `Winlogon` is where consent prompts, the lock screen
/// and Ctrl+Alt+Del live; other desktops exist but nothing puts a session on
/// them.
pub const SECURE_DESKTOP: &str = "Winlogon";

#[cfg(target_os = "windows")]
pub use platform::{
    capture_through_agent_only, release_agent_touches, send_input_to_agent, touch_route_changed,
    use_agent_only, DesktopFollowing,
};

/// Whether this process has no desktop of its own to capture.
#[cfg(not(target_os = "windows"))]
pub fn capture_through_agent_only() -> bool {
    false
}

/// Whether input should be going to an agent rather than injected locally.
#[cfg(not(target_os = "windows"))]
pub fn send_input_to_agent(_event: &crate::protocol::InputEvent) -> bool {
    false
}

/// Whether touch has changed which side is injecting it. Never, without agents.
#[cfg(not(target_os = "windows"))]
pub fn touch_route_changed(_via_agent: bool) -> bool {
    false
}

/// Nothing to ask: there is no agent holding contacts.
#[cfg(not(target_os = "windows"))]
pub fn release_agent_touches() {}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Whether this process must get every frame from an agent.
    ///
    /// # Why a process-wide mode rather than a parameter
    ///
    /// The service has no desktop. It runs in session zero, where Desktop
    /// Duplication has nothing to duplicate, so the local capturer it would
    /// normally wrap does not merely return black — it refuses to start, and
    /// the session fails before a frame is asked for.
    ///
    /// This is not a property of one session or one configuration. It is a fact
    /// about the process: the service is *always* in that position and the
    /// application never is. Threading it through every constructor between
    /// here and where a session is built would be plumbing a constant, so the
    /// service states it once at startup and nothing below has to carry it.
    static AGENT_ONLY: AtomicBool = AtomicBool::new(false);

    /// Say that this process cannot capture anything itself.
    ///
    /// Called by the service before it hosts, and by nothing else.
    pub fn use_agent_only() {
        AGENT_ONLY.store(true, Ordering::Relaxed);
        tracing::info!(
            "This process has no desktop of its own — every frame will come from the \
             {SECURE_DESKTOP} agent"
        );
    }

    /// Whether [`use_agent_only`] was called.
    pub fn capture_through_agent_only() -> bool {
        AGENT_ONLY.load(Ordering::Relaxed)
    }

    /// The link to the secure desktop's agent, while there is one.
    ///
    /// # Why this is shared state and not a field
    ///
    /// The capturer and the injector are built separately, by different parts
    /// of the session, and neither owns the other. They still have to use the
    /// same connection: the pipe accepts one client, and clicks must go
    /// wherever the frames are coming from. A value both can reach is the
    /// honest way to say that, and there is exactly one secure desktop per
    /// machine so there is nothing to key it by.
    static LINK: Mutex<Option<Arc<super::super::agent_pipe::AgentFrames>>> = Mutex::new(None);

    /// A link to the ordinary desktop's agent, for input this process cannot
    /// deliver itself.
    ///
    /// Separate from [`LINK`], which exists only while frames are coming from
    /// the secure desktop. This one carries no frames at all.
    static INPUT_LINK: Mutex<Option<Arc<super::super::agent_pipe::AgentFrames>>> =
        Mutex::new(None);

    /// Where touch contacts were last injected: here, or by the agent.
    ///
    /// Only touch needs this. A click or a keystroke that goes one way and then
    /// the other is two independent events, and neither side is left holding
    /// anything. A contact is state — somebody is holding it down until they
    /// are told otherwise — so when the route changes mid-gesture the side
    /// being left has to be told to let go.
    static TOUCH_VIA_AGENT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Record where touch is going now, and say whether that is a change.
    pub fn touch_route_changed(via_agent: bool) -> bool {
        TOUCH_VIA_AGENT.swap(via_agent, std::sync::atomic::Ordering::Relaxed) != via_agent
    }

    /// Ask whichever agent has been injecting touch to drop its contacts.
    ///
    /// Nothing to report on failure: the agent is gone, which is the same
    /// outcome as it letting go.
    pub fn release_agent_touches() {
        for held in [&LINK, &INPUT_LINK] {
            if let Ok(link) = held.lock()
                && let Some(agent) = link.as_ref()
                && let Err(e) = agent.send_release_touches()
            {
                tracing::debug!("Could not ask the agent to release touches: {e:#}");
            }
        }
    }

    /// Send an event to an agent, if one has to deliver it.
    ///
    /// Returns whether it went. `false` means inject locally as usual, which is
    /// the ordinary case and the fast one.
    ///
    /// # Two reasons an agent has to do it instead
    ///
    /// The first is the secure desktop: this process cannot reach it at all, so
    /// while frames are coming from there the clicks go back the same way.
    ///
    /// The second is an elevated window. `SendInput` cannot reach a window at a
    /// higher integrity level than the process calling it — Windows enforces
    /// that in the kernel, deliberately, and it is why a guest loses control the
    /// moment anything running as administrator takes focus. There is no flag
    /// that lifts it; the only way through is a process that outranks the
    /// window, which is what the agent running as Local System is.
    pub fn send_input_to_agent(event: &crate::protocol::InputEvent) -> bool {
        if let Ok(link) = LINK.lock()
            && let Some(agent) = link.as_ref()
        {
            return match agent.send_input(event) {
                Ok(()) => true,
                Err(e) => {
                    // Not retried and not fatal. The capturer notices the same
                    // failure on its next frame and drops the link; until then,
                    // losing a click beats injecting it on the wrong desktop.
                    tracing::debug!("Could not send input to the agent: {e:#}");
                    false
                }
            };
        }

        if !foreground_outranks_us_cached() {
            return false;
        }
        send_through_input_link(event)
    }

    /// Deliver one event through the ordinary desktop's agent.
    ///
    /// Connects on first need and keeps the link: an elevated window is usually
    /// in front for as long as somebody is using it, and reconnecting per
    /// keystroke would put a pipe handshake in the path of typing.
    fn send_through_input_link(event: &crate::protocol::InputEvent) -> bool {
        let Ok(mut link) = INPUT_LINK.lock() else { return false };

        if link.is_none() {
            match super::super::agent_pipe::connect("Default") {
                Ok(agent) => {
                    let agent = Arc::new(agent);
                    // Explicitly no frames: this link exists to carry input.
                    if let Err(e) = agent.send_frames_wanted(false) {
                        tracing::debug!("Could not open an input link to the agent: {e:#}");
                        return false;
                    }
                    tracing::info!(
                        "An elevated window is in front — sending input through the agent, \
                         which outranks it"
                    );
                    *link = Some(agent);
                }
                Err(e) => {
                    // No service installed, most likely, which is the ordinary
                    // case on a machine nobody has set that up on. Debug rather
                    // than a warning per keystroke.
                    tracing::debug!("No agent to send input through: {e:#}");
                    return false;
                }
            }
        }

        let Some(agent) = link.as_ref() else { return false };
        match agent.send_input(event) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("The input link to the agent failed: {e:#}");
                // Dropped so the next event opens a fresh one rather than
                // failing against a pipe that has gone.
                *link = None;
                false
            }
        }
    }

    /// The last answer from [`foreground_outranks_us`], and when it was taken.
    ///
    /// # Why this is cached at all
    ///
    /// The check is four system calls — the foreground window, its process, two
    /// token queries — and it sits on the path of every input event. Pointer
    /// movement is now forwarded per movement rather than per frame, so that is
    /// hundreds a second on a trackpad, and four syscalls each would be a real
    /// cost for an answer that changes when somebody switches window.
    ///
    /// A fifth of a second is far longer than any input burst and far shorter
    /// than anyone notices. The worst case is a handful of events going the
    /// wrong way immediately after a focus change: on the way in they are lost
    /// to UIPI, which is what happened to all of them before; on the way out
    /// they take a pipe hop they did not need.
    static OUTRANKED: Mutex<Option<(bool, std::time::Instant)>> = Mutex::new(None);

    /// How long an answer about the foreground window stays good for.
    const OUTRANK_CACHE: std::time::Duration = std::time::Duration::from_millis(200);

    fn foreground_outranks_us_cached() -> bool {
        let Ok(mut cached) = OUTRANKED.lock() else { return false };
        if let Some((answer, taken)) = *cached
            && taken.elapsed() < OUTRANK_CACHE
        {
            return answer;
        }
        let answer = foreground_outranks_us();
        *cached = Some((answer, std::time::Instant::now()));
        answer
    }

    /// Whether the window in front belongs to something this process cannot
    /// send input to.
    ///
    /// # Why an unreadable answer counts as yes
    ///
    /// Asking is itself subject to the rule being asked about: a medium
    /// integrity process is often refused the token of a higher one, and that
    /// refusal is the most reliable evidence available. Treating "cannot tell"
    /// as "cannot reach" costs a pipe hop on the occasional system-owned window
    /// and buys working input on every elevated one.
    ///
    /// The alternative was to try `SendInput` and check — and it cannot be
    /// checked. Its documentation is explicit that when UIPI blocks it, neither
    /// the return value nor `GetLastError` says so.
    fn foreground_outranks_us() -> bool {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };

        /// The integrity level of a process, or `None` if it cannot be read.
        fn integrity_of(process: HANDLE) -> Option<u32> {
            // SAFETY: a two-call query into a buffer sized by the first call.
            unsafe {
                let mut token = HANDLE::default();
                OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

                let mut needed = 0u32;
                let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
                let mut buffer = vec![0u8; needed as usize];
                let read = GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    Some(buffer.as_mut_ptr().cast()),
                    needed,
                    &mut needed,
                )
                .is_ok();
                let level = read.then(|| {
                    let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                    let count = *windows::Win32::Security::GetSidSubAuthorityCount(label.Label.Sid);
                    *windows::Win32::Security::GetSidSubAuthority(
                        label.Label.Sid,
                        (count - 1) as u32,
                    )
                });
                let _ = CloseHandle(token);
                level
            }
        }

        // SAFETY: documented reads; the process handle is closed below.
        unsafe {
            let window = GetForegroundWindow();
            if window.is_invalid() {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid == 0 {
                return false;
            }

            let ours = match integrity_of(GetCurrentProcess()) {
                Some(level) => level,
                // Our own token is unreadable, which should not happen and is
                // not a reason to reroute everything.
                None => return false,
            };

            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                // Refused even limited information: not something we can send
                // input to either.
                return true;
            };
            let theirs = integrity_of(process);
            let _ = CloseHandle(process);

            theirs.is_none_or(|level| level > ours)
        }
    }

    /// Wraps a local capturer and falls back to an agent's frames.
    pub struct DesktopFollowing {
        /// `None` in the service, which has no desktop to capture.
        local: Option<Box<dyn ScreenCaptureBackend>>,
        /// Running only while the input is somewhere this process cannot see.
        agent: Option<AgentReader>,
        /// Whether the last look found the input on a desktop we can capture.
        had_local_input: bool,
        /// Frames taken from the agent since it was last connected.
        ///
        /// Counted so the two ends can be told apart in a log. "The agent sent
        /// nothing" and "the host received nothing" look identical from either
        /// side alone, and the first time this failed it took both logs to see
        /// that nothing had failed at all — the picture simply stopped.
        from_agent: u64,
    }

    impl DesktopFollowing {
        pub fn new(local: Box<dyn ScreenCaptureBackend>) -> Self {
            Self { local: Some(local), agent: None, had_local_input: true, from_agent: 0 }
        }

        /// A capturer with no desktop of its own, for the service.
        ///
        /// Every frame comes from the agent. There is no local backend to fall
        /// back to and no question about which desktop to ask: with nobody
        /// signed in, the sign-in screen *is* the secure desktop, and it is the
        /// only thing there is to show.
        pub fn agent_only() -> Self {
            Self { local: None, agent: None, had_local_input: false, from_agent: 0 }
        }

        /// Whether this process can still see the desktop receiving input.
        ///
        /// `OpenInputDesktop` refusing is the signal, not an error: a process on
        /// the ordinary desktop is not permitted to open the secure one, so
        /// being turned away is precisely how it learns the secure desktop is
        /// up. There is no positive notification of this anywhere in Windows.
        fn local_has_input() -> bool {
            // A process with no desktop never has the input, whatever Windows
            // would say about session zero's own.
            if capture_through_agent_only() {
                return false;
            }
            use windows::Win32::System::StationsAndDesktops::{
                CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
            };

            // SAFETY: documented; the handle is closed immediately.
            unsafe {
                match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
                    Ok(handle) => {
                        let _ = CloseDesktop(handle);
                        true
                    }
                    Err(_) => false,
                }
            }
        }
    }

    impl ScreenCaptureBackend for DesktopFollowing {
        fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
            let local = Self::local_has_input();

            if local != self.had_local_input {
                self.had_local_input = local;
                if local {
                    tracing::info!(
                        "The input is back on this desktop — capturing here again ({} frames came from the agent)",
                        self.from_agent,
                    );
                    self.from_agent = 0;
                    // Dropping it closes the pipe, which is what tells the
                    // agent to stop capturing. The link goes first, so no
                    // click can be sent down a pipe that is about to close.
                    if let Ok(mut link) = LINK.lock() {
                        *link = None;
                    }
                    self.agent = None;
                } else {
                    tracing::info!(
                        "The input has moved to a desktop this process cannot see — \
                         asking the {SECURE_DESKTOP} agent for frames"
                    );
                }
            }

            if local && let Some(local) = self.local.as_mut() {
                return local.capture_next();
            }

            // Connecting is retried on each call rather than once, because the
            // agent may not have its pipe up at the instant the desktop
            // switches, and the alternative is giving up for the whole prompt.
            if self.agent.is_none() {
                match AgentReader::start(SECURE_DESKTOP) {
                    Ok(reader) => self.agent = Some(reader),
                    Err(e) => {
                        tracing::debug!("No frames from the {SECURE_DESKTOP} agent yet: {e:#}");
                        // Not an error to the caller. The session carries on
                        // with the last frame it had, which is what it would do
                        // anyway, and recovers when the prompt closes.
                        return Ok(None);
                    }
                }
            }

            let Some(reader) = self.agent.as_mut() else { return Ok(None) };
            match reader.take() {
                Some(frame) => {
                    self.from_agent += 1;
                    if self.from_agent == 1 {
                        tracing::info!(
                            "First frame from the {SECURE_DESKTOP} agent: {}x{}",
                            frame.width,
                            frame.height,
                        );
                    }
                    Ok(Some(frame))
                }
                None if reader.finished() => {
                    tracing::info!("The {SECURE_DESKTOP} agent stopped sending");
                    if let Ok(mut link) = LINK.lock() {
                        *link = None;
                    }
                    self.agent = None;
                    Ok(None)
                }
                None => Ok(None),
            }
        }

        fn take_cursor(&mut self) -> Option<super::super::cursor::CursorUpdate> {
            // Only from the local backend. The agent's pointer is drawn into
            // its own frames, and reporting a second one would put two on the
            // guest's screen.
            self.local
                .as_mut()
                .filter(|_| self.had_local_input)
                .and_then(|local| local.take_cursor())
        }

        fn frame_size(&self) -> (u16, u16) {
            // Without a local backend the size is whatever the agent sends, and
            // nothing has arrived yet. The primary display's size is the right
            // guess: the agent is capturing the same screen.
            self.local.as_ref().map_or_else(
                super::super::screen_capture::primary_display_size,
                |local| local.frame_size(),
            )
        }

        fn set_scale(&mut self, scale: QualityScale) -> bool {
            self.local.as_mut().is_some_and(|local| local.set_scale(scale))
        }

        fn request_format(&mut self, format: PixelFormat) -> bool {
            // Agent frames arrive as the agent captured them, which is BGRA.
            // Accepting a negotiated NV12 here would be a promise this cannot
            // keep the moment a consent prompt appears.
            if format != PixelFormat::Bgra {
                return false;
            }
            self.local.as_mut().is_some_and(|local| local.request_format(format))
        }

        fn is_real(&self) -> bool {
            // Agent frames are a real screen even though this process cannot
            // see one. Reporting otherwise would have the session treat them
            // as a test pattern.
            self.local.as_ref().is_none_or(|local| local.is_real())
        }
    }

    /// Reads an agent's frames on its own thread, keeping only the newest.
    ///
    /// The pipe read blocks until a frame arrives, and a consent prompt is
    /// mostly still, so a blocking read on the capture thread would stall the
    /// session for as long as nobody moved the mouse. Reading elsewhere and
    /// handing over the latest keeps the frame loop's own timing intact — and
    /// dropping older frames is what the loop does with its own capturer
    /// anyway.
    struct AgentReader {
        latest: Arc<Mutex<Option<ScreenFrame>>>,
        done: Arc<AtomicBool>,
    }

    impl AgentReader {
        fn start(desktop: &str) -> Result<Self> {
            let frames = Arc::new(super::super::agent_pipe::connect(desktop)?);
            // Frames are opt-in now, because the same pipe carries input-only
            // links that must not start a duplication. See `PipeRequest`.
            frames.send_frames_wanted(true)?;
            let latest = Arc::new(Mutex::new(None));
            let done = Arc::new(AtomicBool::new(false));

            // Published before the reader starts, so the first click cannot
            // arrive before there is anywhere to send it.
            if let Ok(mut link) = LINK.lock() {
                *link = Some(frames.clone());
            }

            let into = latest.clone();
            let finished = done.clone();
            let reading = frames.clone();
            std::thread::Builder::new()
                .name("tain-agent-frames".into())
                .spawn(move || {
                    loop {
                        match reading.next_frame() {
                            Ok(frame) => {
                                let screen = ScreenFrame::with_format(
                                    frame.width,
                                    frame.height,
                                    frame.data.into(),
                                    PixelFormat::Bgra,
                                    std::time::Duration::ZERO,
                                );
                                if let Ok(mut slot) = into.lock() {
                                    *slot = Some(screen);
                                }
                            }
                            Err(e) => {
                                tracing::debug!("Agent frames ended: {e:#}");
                                finished.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                })?;

            Ok(Self { latest, done })
        }

        /// The newest frame, if one has arrived since the last call.
        fn take(&mut self) -> Option<ScreenFrame> {
            self.latest.lock().ok().and_then(|mut slot| slot.take())
        }

        fn finished(&self) -> bool {
            self.done.load(Ordering::Relaxed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a *change* of route may release contacts.
    ///
    /// It is asked once per touch event, which during a drag is every few
    /// milliseconds. If a steady route reported a change, every one of those
    /// would cancel the gesture it was in the middle of.
    #[test]
    #[cfg(target_os = "windows")]
    fn a_steady_touch_route_reports_no_change() {
        // Whatever the process was doing, start from a known side.
        touch_route_changed(false);

        assert!(!touch_route_changed(false), "staying local is not a change");
        assert!(touch_route_changed(true), "moving to the agent is");
        assert!(!touch_route_changed(true), "and then staying there is not");
        assert!(touch_route_changed(false), "coming back is a change again");

        // Left as found, since this is process-wide state.
        touch_route_changed(false);
    }
}
