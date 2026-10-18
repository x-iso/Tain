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
//!
//! # Two processes use this, as two different users
//!
//! The application, as the signed-in user, and the unattended host, as Local
//! System — both on the ordinary desktop. That difference is why "does this
//! desktop have the input" is asked by name rather than by whether the input
//! desktop can be opened: see [`this_desktop_has_input`]. And the host starts
//! on a machine that is often locked, which is why the local backend can be
//! built after the fact: see `DesktopFollowing::deferred`.

use anyhow::Result;

use super::screen_capture::{PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame};

/// The desktop the secure agent serves.
///
/// Only one is followed. `Winlogon` is where consent prompts, the lock screen
/// and Ctrl+Alt+Del live; other desktops exist but nothing puts a session on
/// them.
pub const SECURE_DESKTOP: &str = "Winlogon";

/// The desktop a signed-in user's session lives on.
///
/// Where the application runs, where the unattended host is started, and the
/// agent that carries input to elevated windows.
pub const ORDINARY_DESKTOP: &str = "Default";

#[cfg(target_os = "windows")]
pub use platform::{
    input_desktop_name, own_desktop_name, release_agent_touches, send_input_to_agent,
    this_desktop_has_input, touch_route_changed, DesktopFollowing, LocalFactory,
};

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

    /// The name of the desktop currently receiving input, if it can be opened.
    ///
    /// `None` is an answer, not a failure: a process running as the signed-in
    /// user cannot open the secure desktop, so being refused is how it learns
    /// that a consent prompt or the lock screen is up.
    pub fn input_desktop_name() -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
        };

        // SAFETY: documented; the handle is closed before returning.
        unsafe {
            let handle =
                OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
            let name = desktop_name(handle);
            let _ = CloseDesktop(handle);
            name
        }
    }

    /// The name of the desktop this process was started on.
    ///
    /// Asked once and kept. A process cannot move to another desktop, and no
    /// thread in this one ever calls `SetThreadDesktop`, so the first answer is
    /// the answer for its whole life.
    pub fn own_desktop_name() -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::GetThreadDesktop;
        use windows::Win32::System::Threading::GetCurrentThreadId;

        static OWN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        OWN.get_or_init(|| {
            // SAFETY: the handle belongs to the thread and must not be closed.
            let handle = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok()?;
            desktop_name(handle)
        })
        .clone()
    }

    fn desktop_name(handle: windows::Win32::System::StationsAndDesktops::HDESK) -> Option<String> {
        use windows::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

        let mut buffer = [0u16; 128];
        let mut needed = 0u32;
        // SAFETY: a documented read into a buffer this function owns, sized in
        // bytes as the call expects.
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
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }

    /// Whether the desktop receiving input is the one this process is on.
    ///
    /// # By name, because being able to open it proves nothing
    ///
    /// This used to ask only whether `OpenInputDesktop` succeeded. For the
    /// application that is the same question: running as the signed-in user it
    /// is refused the secure desktop, so success meant the input was here.
    ///
    /// The unattended host runs on the same desktop as Local System, and Local
    /// System is not refused anything. It opens the secure desktop as easily as
    /// its own — so it would have gone on believing the input was here through
    /// every consent prompt and every lock, duplicating a desktop Windows was
    /// not showing. Comparing the name is the question that was always meant,
    /// and it gives the same answer for the application as the old test did.
    pub fn this_desktop_has_input() -> bool {
        let Some(input) = input_desktop_name() else { return false };
        own_desktop_name().is_some_and(|own| own.eq_ignore_ascii_case(&input))
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
            match super::super::agent_pipe::connect(ORDINARY_DESKTOP) {
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

    /// Builds the capturer for this process's own desktop, for a wrapper that
    /// could not have one when it was made. See [`DesktopFollowing::deferred`].
    pub type LocalFactory = Box<dyn Fn() -> Result<Box<dyn ScreenCaptureBackend>> + Send + Sync>;

    /// Wraps a local capturer and falls back to an agent's frames.
    pub struct DesktopFollowing {
        /// The capturer for this desktop. `None` until it can be built — see
        /// [`Self::deferred`].
        local: Option<Box<dyn ScreenCaptureBackend>>,
        /// How to build `local`, while there is not one yet.
        build_local: Option<LocalFactory>,
        /// When building `local` last failed, so a desktop that has just come
        /// back and is not ready to be duplicated is not asked on every frame.
        build_failed_at: Option<std::time::Instant>,
        /// What the session has asked of the capturer, so one built later is
        /// told as well.
        asked: Asked,
        /// Running only while the input is somewhere this process cannot see.
        agent: Option<AgentSubscription>,
        /// Whether the last look found the input on a desktop we can capture.
        had_local_input: bool,
        /// Frames taken from the agent since it was last connected.
        ///
        /// Counted so the two ends can be told apart in a log. "The agent sent
        /// nothing" and "the host received nothing" look identical from either
        /// side alone, and the first time this failed it took both logs to see
        /// that nothing had failed at all — the picture simply stopped.
        from_agent: u64,
        /// Whether this desktop has the input. A function rather than a call so
        /// a test can answer it; see [`this_desktop_has_input`] for the real one.
        has_input: fn() -> bool,
    }

    /// The session's requests of a capturer that does not exist yet.
    ///
    /// # Why they are remembered rather than refused
    ///
    /// The session asks for a scale, a size ceiling, a granularity and a format
    /// once, when it starts, and does not ask again unless something changes. A
    /// capturer built afterwards would never hear them — so a session that
    /// began while the machine was locked would, on unlock, duplicate at full
    /// native resolution in BGRA for the rest of its life, ignoring the quality
    /// setting and the guest's window. Replaying them onto the capturer when it
    /// is built makes a late start the same as an early one.
    #[derive(Default)]
    struct Asked {
        scale: Option<QualityScale>,
        ceiling: Option<Option<(u16, u16)>>,
        granularity: Option<(u16, u16)>,
        format: Option<PixelFormat>,
    }

    impl Asked {
        fn replay(&self, capturer: &mut dyn ScreenCaptureBackend) {
            if let Some(scale) = self.scale {
                capturer.set_scale(scale);
            }
            if let Some(ceiling) = self.ceiling {
                capturer.set_size_ceiling(ceiling);
            }
            if let Some(granularity) = self.granularity {
                capturer.set_size_granularity(granularity);
            }
            if let Some(format) = self.format {
                capturer.request_format(format);
            }
        }
    }

    /// How long to wait before trying again to build a local capturer.
    ///
    /// The input returning to a desktop and Desktop Duplication agreeing to
    /// open it are not the same instant. Half a second is a retry nobody sees
    /// and an attempt nobody pays for sixty times a second.
    const RETRY_BUILDING_LOCAL: std::time::Duration = std::time::Duration::from_millis(500);

    impl DesktopFollowing {
        pub fn new(local: Box<dyn ScreenCaptureBackend>) -> Self {
            Self {
                local: Some(local),
                build_local: None,
                build_failed_at: None,
                asked: Asked::default(),
                agent: None,
                had_local_input: true,
                from_agent: 0,
                has_input: this_desktop_has_input,
            }
        }

        /// A capturer whose own desktop could not be duplicated when it was
        /// made, because another desktop had the input.
        ///
        /// It shows the secure desktop's agent until its own desktop is back in
        /// front, then builds its local capturer with `build` and carries on as
        /// though it had had one all along. See `create_capturer` for when this
        /// happens, which for the unattended host is every time the machine is
        /// locked.
        pub fn deferred(build: LocalFactory) -> Self {
            Self {
                local: None,
                build_local: Some(build),
                build_failed_at: None,
                asked: Asked::default(),
                agent: None,
                had_local_input: false,
                from_agent: 0,
                has_input: this_desktop_has_input,
            }
        }

        /// Try to build the local capturer, if there is not one and it is time.
        fn build_local_if_due(&mut self) {
            if self.local.is_some() {
                return;
            }
            let Some(build) = self.build_local.as_ref() else { return };
            if self.build_failed_at.is_some_and(|at| at.elapsed() < RETRY_BUILDING_LOCAL) {
                return;
            }
            match build() {
                Ok(mut capturer) => {
                    self.asked.replay(&mut *capturer);
                    tracing::info!("This desktop has the input again — capturing it directly");
                    self.local = Some(capturer);
                    self.build_local = None;
                    self.build_failed_at = None;
                }
                Err(e) => {
                    // Once per run of failures, not twice a second.
                    if self.build_failed_at.is_none() {
                        tracing::info!("This desktop cannot be captured yet: {e:#}");
                    }
                    self.build_failed_at = Some(std::time::Instant::now());
                }
            }
        }

        /// The same wrapper, deciding whether its desktop has the input with
        /// `has_input` instead of asking Windows. Tests only.
        #[cfg(test)]
        pub fn answering_input_with(mut self, has_input: fn() -> bool) -> Self {
            self.has_input = has_input;
            self
        }

        /// Whether a request made now will reach a capturer.
        ///
        /// A wrapper that is waiting to build one says yes, because it will be
        /// honoured the moment there is something to honour it — and until
        /// then the frames come from the agent, which is the same exception a
        /// consent prompt has always been. Saying no instead would have the
        /// session wrap this in a CPU resampler for its whole life, long after
        /// the GPU was available to do the same work for nothing.
        fn will_be_honoured(&self) -> bool {
            self.build_local.is_some()
        }
    }

    impl ScreenCaptureBackend for DesktopFollowing {
        fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
            let local = (self.has_input)();

            if local != self.had_local_input {
                self.had_local_input = local;
                if local {
                    tracing::info!(
                        "The input is back on this desktop — capturing here again ({} frames came from the agent)",
                        self.from_agent,
                    );
                    self.from_agent = 0;
                    // Letting go. When this was the last session holding the
                    // link, the link closes — withdrawing itself as the route
                    // for input first, then really closing the pipe, which is
                    // what frees it for the next time. See `AgentLink`.
                    self.agent = None;
                } else {
                    tracing::info!(
                        "The input has moved to a desktop this process cannot see — \
                         asking the {SECURE_DESKTOP} agent for frames"
                    );
                }
            }

            if local {
                self.build_local_if_due();
                return match self.local.as_mut() {
                    Some(capturer) => capturer.capture_next(),
                    // The input is here and the capturer is not up yet. There
                    // is nothing to show from anywhere: the agent serves the
                    // other desktop, which no longer has anything on it.
                    None => Ok(None),
                };
            }

            // Connecting is retried on each call rather than once, because the
            // agent may not have its pipe up at the instant the desktop
            // switches, and the alternative is giving up for the whole prompt.
            if self.agent.is_none() {
                match AgentLink::attach(&SECURE_AGENT, SECURE_DESKTOP) {
                    // From generation zero, so a session joining a link that
                    // is already open is handed the picture that is already
                    // there. Desktop Duplication sends only changes, and a lock
                    // screen does not change — waiting for the next frame would
                    // mean waiting for somebody to move the mouse.
                    Ok(link) => self.agent = Some(AgentSubscription { link, seen: 0 }),
                    Err(e) => {
                        tracing::debug!("No frames from the {SECURE_DESKTOP} agent yet: {e:#}");
                        // Not an error to the caller. The session carries on
                        // with the last frame it had, which is what it would do
                        // anyway, and recovers when the prompt closes.
                        return Ok(None);
                    }
                }
            }

            let Some(subscription) = self.agent.as_mut() else { return Ok(None) };
            // A frame that arrived before the agent went is still a frame, so
            // it is taken before asking whether the agent has gone.
            if let Some((generation, frame)) = subscription.link.newer_than(subscription.seen) {
                subscription.seen = generation;
                self.from_agent += 1;
                if self.from_agent == 1 {
                    tracing::info!(
                        "First frame from the {SECURE_DESKTOP} agent: {}x{}",
                        frame.width,
                        frame.height,
                    );
                }
                return Ok(Some(frame));
            }
            if subscription.link.finished() {
                tracing::info!("The {SECURE_DESKTOP} agent stopped sending");
                // Letting go of a finished link is what allows the next call to
                // open a new one rather than rejoin the dead one.
                self.agent = None;
            }
            Ok(None)
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
            // Without a local backend yet, the size is whatever the agent
            // sends, and nothing may have arrived. The primary display's size
            // is the right guess: the agent is capturing the same screen.
            self.local.as_ref().map_or_else(
                super::super::screen_capture::primary_display_size,
                |local| local.frame_size(),
            )
        }

        fn set_scale(&mut self, scale: QualityScale) -> bool {
            self.asked.scale = Some(scale);
            match self.local.as_mut() {
                Some(local) => local.set_scale(scale),
                None => self.will_be_honoured(),
            }
        }

        /// # This was missing, and every Windows host paid for it
        ///
        /// The trait's default answers "not honoured" and does nothing, and
        /// this wrapper sits in front of the local capturer on **every** host
        /// session — so the guest's viewport, the one cap that knows how many
        /// pixels are actually going to be looked at, reached nothing. A host
        /// streamed the quality slider's fraction of its native size and no
        /// less, and a guest in a 1856x1020 window was sent 2880x1620 off a 4K
        /// screen: two and a half times the pixels, paid for in capture,
        /// colour conversion, encode, bitrate, decode and upload, and then
        /// scaled away before anyone saw them.
        ///
        /// Invisible because it fails quiet: the composed wall implements it,
        /// so "every screen" was capped correctly while single-screen capture
        /// was not, which reads as the wall being fast rather than the rest
        /// being slow.
        ///
        /// The agent's own frames are not affected: it sends what it sends, and
        /// the size follows the desktop back when the input does.
        fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
            self.asked.ceiling = Some(ceiling);
            match self.local.as_mut() {
                Some(local) => local.set_size_ceiling(ceiling),
                None => self.will_be_honoured(),
            }
        }

        /// Same delegation, and the same exception for the agent's frames.
        fn set_size_granularity(&mut self, granularity: (u16, u16)) -> bool {
            self.asked.granularity = Some(granularity);
            match self.local.as_mut() {
                Some(local) => local.set_size_granularity(granularity),
                None => self.will_be_honoured(),
            }
        }

        /// Forwarded, or this wrapper hides a wall's staleness from the session
        /// that has to act on it — which is how `set_size_ceiling` went missing.
        fn layout_changed(&self) -> bool {
            self.local.as_ref().is_some_and(|local| local.layout_changed())
        }

        /// Forwarded for the same reason, and it matters more: a composite of
        /// windows that lost its tiles here would have every click mapped
        /// through the primary screen instead.
        fn input_tiles(&self) -> Option<Vec<crate::input_inject::Tile>> {
            self.local.as_ref().and_then(|local| local.input_tiles())
        }

        fn request_format(&mut self, format: PixelFormat) -> bool {
            // # Why a mixed stream is not a broken promise
            //
            // This refused anything but BGRA, reasoning that NV12 could not be
            // promised when a consent prompt might hand capture to the agent at
            // any moment — the agent reads the secure desktop the ordinary way
            // and has no GPU converter to offer.
            //
            // Nothing downstream was holding it to that promise. A frame says
            // what it is, and every consumer reads that field per frame rather
            // than the negotiated answer: the encoder converts BGRA and passes
            // NV12 straight through, and the preview takes the format as an
            // argument. `negotiate_format`'s return value reaches exactly one
            // place, a log line.
            //
            // So the refusal bought nothing and cost every Windows host a
            // full-resolution readback and a CPU colour pass on every frame —
            // fourteen megabytes each at 2560x1440 — to avoid a mismatch, on
            // the few seconds a prompt is up, that would not have been one.
            //
            // The agent's own frames still cost the conversion. They are rare,
            // brief, and mostly a still dialog.
            self.asked.format = Some(format);
            match self.local.as_mut() {
                Some(local) => local.request_format(format),
                None => self.will_be_honoured(),
            }
        }

        fn is_real(&self) -> bool {
            // Agent frames are a real screen even while this process has no
            // capturer of its own. Reporting otherwise would have the session
            // treat them as a test pattern.
            self.local.as_ref().is_none_or(|local| local.is_real())
        }
    }

    /// The link to the secure desktop's agent that sessions in this process
    /// share, while any of them wants it.
    static SECURE_AGENT: Mutex<std::sync::Weak<AgentLink>> = Mutex::new(std::sync::Weak::new());

    /// One session's hold on the shared link, and the last frame it was given.
    struct AgentSubscription {
        link: Arc<AgentLink>,
        seen: u64,
    }

    /// How long a closing link waits for its reader to let go of the pipe.
    ///
    /// Cancelling the read takes effect in microseconds. This is the ceiling
    /// for a reader that is somewhere a cancel cannot reach, not a wait anybody
    /// pays.
    const CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

    /// A connection to an agent, reading its frames on a thread of its own.
    ///
    /// # Why reading has a thread
    ///
    /// The pipe read blocks until a frame arrives, and a consent prompt is
    /// mostly still, so a blocking read on the capture thread would stall the
    /// session for as long as nobody moved the mouse. Reading elsewhere and
    /// handing over the latest keeps the frame loop's own timing intact — and
    /// dropping older frames is what the loop does with its own capturer
    /// anyway.
    ///
    /// # Why there is one per process, shared
    ///
    /// The agent's pipe has a single instance, because an agent serves one
    /// host. A host can have several sessions, though — two guests at once, or
    /// a guest that reconnected while its old session was still winding down —
    /// and each used to open a connection of its own. Whichever got there first
    /// had the secure desktop, and every other session was told the pipe was
    /// busy and showed a frozen picture until the input came back.
    ///
    /// # Why closing it is work
    ///
    /// The reading thread holds the pipe, and is almost always blocked in that
    /// read — the agent sends nothing once the input leaves its desktop, which
    /// is precisely when a session lets go. Dropping every other reference
    /// therefore closed nothing. The thread waited on, the agent went on
    /// holding the connection, and the next attempt to connect found the pipe
    /// busy: the first lock worked and every later one froze, until the input
    /// came back to a desktop the host could capture itself. So the last holder
    /// to leave cancels the read, and the pipe closes when the thread returns.
    pub(super) struct AgentLink {
        frames: Arc<super::super::agent_pipe::AgentFrames>,
        /// The newest frame, numbered, so each session can tell whether it has
        /// already had it without taking it away from the others.
        latest: Arc<Mutex<(u64, Option<ScreenFrame>)>>,
        /// Asks the reader to stop.
        stop: Arc<AtomicBool>,
        /// Set by the reader once it has stopped, for whatever reason.
        done: Arc<AtomicBool>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    impl AgentLink {
        /// The link in `slot`, opening one to the agent on `desktop` if there
        /// is no live one to join.
        pub(super) fn attach(
            slot: &Mutex<std::sync::Weak<AgentLink>>,
            desktop: &str,
        ) -> Result<Arc<AgentLink>> {
            let mut shared =
                slot.lock().map_err(|_| anyhow::anyhow!("the shared agent link was poisoned"))?;
            if let Some(link) = shared.upgrade()
                && !link.finished()
            {
                return Ok(link);
            }
            let link = Arc::new(Self::open(desktop)?);
            *shared = Arc::downgrade(&link);
            Ok(link)
        }

        fn open(desktop: &str) -> Result<Self> {
            let frames = Arc::new(super::super::agent_pipe::connect(desktop)?);
            // Frames are opt-in, because the same pipe carries input-only links
            // that must not start a duplication. See `PipeRequest`.
            frames.send_frames_wanted(true)?;

            let latest = Arc::new(Mutex::new((0u64, None)));
            let stop = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicBool::new(false));

            // Published before the reader starts, so the first click cannot
            // arrive before there is anywhere to send it.
            if let Ok(mut link) = LINK.lock() {
                *link = Some(frames.clone());
            }

            let (reading, into, stopping, finished) =
                (frames.clone(), latest.clone(), stop.clone(), done.clone());
            let spawned = std::thread::Builder::new().name("tain-agent-frames".into()).spawn(
                move || {
                    while !stopping.load(Ordering::Relaxed) {
                        match reading.next_frame() {
                            Ok(frame) => {
                                // The format the agent says it sent. This was
                                // `Bgra` whatever the byte said, which would
                                // have turned the first NV12 frame an agent
                                // produced into a picture of noise.
                                let Some(format) = PixelFormat::from_wire(frame.format) else {
                                    tracing::debug!(
                                        "Skipping an agent frame in unknown format {}",
                                        frame.format
                                    );
                                    continue;
                                };
                                let screen = ScreenFrame::with_format(
                                    frame.width,
                                    frame.height,
                                    frame.data.into(),
                                    format,
                                    std::time::Duration::ZERO,
                                );
                                if let Ok(mut slot) = into.lock() {
                                    *slot = (slot.0 + 1, Some(screen));
                                }
                            }
                            Err(e) => {
                                // A cancelled read is how a closing link stops
                                // this, and is not worth a line.
                                if !stopping.load(Ordering::Relaxed) {
                                    tracing::debug!("Agent frames ended: {e:#}");
                                }
                                break;
                            }
                        }
                    }
                    finished.store(true, Ordering::Relaxed);
                },
            );

            match spawned {
                Ok(reader) => Ok(Self { frames, latest, stop, done, reader: Some(reader) }),
                Err(e) => {
                    // Withdrawn again: a route for input with nothing behind it
                    // would send every click to a link that is about to close.
                    if let Ok(mut link) = LINK.lock() {
                        *link = None;
                    }
                    Err(anyhow::Error::new(e).context("could not start reading the agent's frames"))
                }
            }
        }

        /// The newest frame, if it is newer than `seen`, with its number.
        ///
        /// Cloned rather than taken, because other sessions may want it too.
        /// A frame's pixels are shared, so the clone is a reference count.
        pub(super) fn newer_than(&self, seen: u64) -> Option<(u64, ScreenFrame)> {
            let slot = self.latest.lock().ok()?;
            let (generation, frame) = &*slot;
            if *generation <= seen {
                return None;
            }
            frame.clone().map(|frame| (*generation, frame))
        }

        /// Whether the reader has stopped — the agent went, or the pipe broke.
        pub(super) fn finished(&self) -> bool {
            self.done.load(Ordering::Relaxed)
        }

        /// The connection itself. Tests only.
        #[cfg(test)]
        pub(super) fn frames(&self) -> &super::super::agent_pipe::AgentFrames {
            &self.frames
        }
    }

    impl Drop for AgentLink {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);

            // Withdrawn as the route for input before the pipe goes, so no
            // click is sent down a link that is closing. Only if it is this
            // link: a newer one may already have taken its place.
            if let Ok(mut link) = LINK.lock()
                && link.as_ref().is_some_and(|held| Arc::ptr_eq(held, &self.frames))
            {
                *link = None;
            }

            // Said first, so the agent lets go of its duplication even in the
            // moment before it notices the connection has gone. It cannot wait
            // behind the reader's pending read: the link is overlapped for
            // exactly this. See `AgentFrames`.
            let _ = self.frames.send_frames_wanted(false);

            let Some(reader) = self.reader.take() else { return };
            // Repeated, because a cancel can land in the moment between two
            // reads and the next read would wait again. A cancel with nothing
            // to cancel is harmless.
            let started = std::time::Instant::now();
            while !reader.is_finished() && started.elapsed() < CLOSE_WAIT {
                self.frames.cancel_pending();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if reader.is_finished() {
                let _ = reader.join();
            } else {
                // Left to finish on its own. It holds the pipe until the agent
                // closes its end, which the agent now does when it notices this
                // side has stopped listening.
                tracing::warn!(
                    "The agent frame reader did not stop within {CLOSE_WAIT:?} — its connection \
                     will close when the agent notices"
                );
            }
            // `frames` closes as this returns, if the reader has let go of it.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An NV12 request must reach the local backend rather than stop here.
    ///
    /// This wrapper refused every format but BGRA, reasoning that a consent
    /// prompt could hand capture to the agent — which produces only BGRA — at
    /// any moment. The price was the GPU colour conversion, on every Windows
    /// host, for every frame of every session. See `request_format` for why the
    /// mixed stream it was avoiding was never a problem.
    #[test]
    #[cfg(target_os = "windows")]
    fn an_nv12_request_reaches_the_local_backend() {
        use std::sync::{Arc, Mutex};

        struct Local {
            asked: Arc<Mutex<Vec<PixelFormat>>>,
            answer: bool,
        }

        impl ScreenCaptureBackend for Local {
            fn capture_next(&mut self) -> anyhow::Result<Option<ScreenFrame>> {
                Ok(None)
            }

            fn frame_size(&self) -> (u16, u16) {
                (64, 64)
            }

            fn request_format(&mut self, format: PixelFormat) -> bool {
                self.asked.lock().unwrap().push(format);
                self.answer
            }
        }

        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut following =
            DesktopFollowing::new(Box::new(Local { asked: Arc::clone(&asked), answer: true }));
        assert!(
            following.request_format(PixelFormat::Nv12),
            "a backend that can convert on the GPU has to be allowed to"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![PixelFormat::Nv12],
            "and has to be asked for the format that was actually wanted"
        );

        // Still the backend's answer, not this wrapper's — delegating is not
        // the same as agreeing.
        let mut refusing = DesktopFollowing::new(Box::new(Local { asked, answer: false }));
        assert!(!refusing.request_format(PixelFormat::Nv12));
    }

    /// The guest's viewport must reach the local backend, not stop here.
    ///
    /// It stopped here for the life of this wrapper, silently: the trait's
    /// default answers "not honoured" and does nothing, so every Windows host
    /// streamed the quality slider's fraction of its native size however small
    /// the window looking at it. See `set_size_ceiling` for what that cost.
    #[test]
    #[cfg(target_os = "windows")]
    fn the_guest_viewport_reaches_the_local_backend() {
        use std::sync::{Arc, Mutex};

        type Asked = Arc<Mutex<Vec<Option<(u16, u16)>>>>;

        struct Local {
            asked: Asked,
        }

        impl ScreenCaptureBackend for Local {
            fn capture_next(&mut self) -> anyhow::Result<Option<ScreenFrame>> {
                Ok(None)
            }

            fn frame_size(&self) -> (u16, u16) {
                (64, 64)
            }

            fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
                self.asked.lock().unwrap().push(ceiling);
                true
            }
        }

        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut following = DesktopFollowing::new(Box::new(Local { asked: Arc::clone(&asked) }));
        assert!(following.set_size_ceiling(Some((1856, 1020))));
        assert!(following.set_size_ceiling(None), "and removing it has to arrive too");
        assert_eq!(*asked.lock().unwrap(), vec![Some((1856, 1020)), None]);
    }

    /// A capturer that could not start is built once its desktop has the input,
    /// and not hammered while it still cannot be.
    ///
    /// This is the locked machine: the unattended host's session starts while
    /// the lock screen has the input, so Desktop Duplication refuses, and the
    /// picture has to begin the moment somebody unlocks rather than never.
    #[test]
    #[cfg(target_os = "windows")]
    fn a_capturer_that_could_not_start_is_built_when_its_desktop_is_back() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Local;
        impl ScreenCaptureBackend for Local {
            fn capture_next(&mut self) -> anyhow::Result<Option<ScreenFrame>> {
                Ok(Some(ScreenFrame::with_format(
                    2,
                    2,
                    vec![0u8; 16].into(),
                    PixelFormat::Bgra,
                    std::time::Duration::ZERO,
                )))
            }
            fn frame_size(&self) -> (u16, u16) {
                (2, 2)
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let mut following = DesktopFollowing::deferred(Box::new(move || {
            // The desktop has just come back and is not ready the first time,
            // which is the ordinary shape of an unlock.
            match counted.fetch_add(1, Ordering::Relaxed) {
                0 => anyhow::bail!("DXGI_ERROR_ACCESS_DENIED"),
                _ => Ok(Box::new(Local) as Box<dyn ScreenCaptureBackend>),
            }
        }))
        .answering_input_with(|| true);

        assert!(following.capture_next().unwrap().is_none(), "nothing yet: the build failed");
        assert!(following.capture_next().unwrap().is_none());
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "a failed build is not retried on the very next frame"
        );

        std::thread::sleep(std::time::Duration::from_millis(600));
        let frame = following.capture_next().unwrap();
        assert!(frame.is_some(), "once it can be built, this desktop's own frames arrive");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        following.capture_next().unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "and it is built once, not per frame");
    }

    /// What the session asked for before there was a capturer reaches the one
    /// built later.
    ///
    /// The session sets its scale, its size ceiling and its format once, at
    /// the start. Without the replay, a session that began while the machine
    /// was locked would duplicate at native size in BGRA for the rest of its
    /// life — which is exactly what the unattended host used to do on every
    /// session, for a different reason.
    #[test]
    #[cfg(target_os = "windows")]
    fn requests_made_before_the_capturer_exists_reach_it() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Seen {
            ceiling: Vec<Option<(u16, u16)>>,
            format: Vec<PixelFormat>,
        }

        struct Local(Arc<Mutex<Seen>>);
        impl ScreenCaptureBackend for Local {
            fn capture_next(&mut self) -> anyhow::Result<Option<ScreenFrame>> {
                Ok(None)
            }
            fn frame_size(&self) -> (u16, u16) {
                (64, 64)
            }
            fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
                self.0.lock().unwrap().ceiling.push(ceiling);
                true
            }
            fn request_format(&mut self, format: PixelFormat) -> bool {
                self.0.lock().unwrap().format.push(format);
                true
            }
        }

        let seen = Arc::new(Mutex::new(Seen::default()));
        let given = Arc::clone(&seen);
        let mut following = DesktopFollowing::deferred(Box::new(move || {
            Ok(Box::new(Local(Arc::clone(&given))) as Box<dyn ScreenCaptureBackend>)
        }))
        .answering_input_with(|| true);

        // Said yes to, because they will be honoured — see `will_be_honoured`.
        assert!(following.set_size_ceiling(Some((1856, 1020))));
        assert!(following.request_format(PixelFormat::Nv12));
        assert!(seen.lock().unwrap().ceiling.is_empty(), "nothing to tell yet");

        following.capture_next().unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.ceiling, vec![Some((1856, 1020))], "the guest's window reached it");
        assert_eq!(seen.format, vec![PixelFormat::Nv12], "and so did the GPU format");
    }

    /// A link every session has let go of really closes, and the agent's one
    /// pipe is free for the next — the second lock that froze.
    ///
    /// Run against a real named pipe served the way the agents serve theirs:
    /// one instance, a frame when frames are asked for and nothing after, and
    /// no way to learn that the host has left except through its input. That
    /// is an agent whose desktop has just lost the input, which is exactly when
    /// a host lets go. Before the fix, the host's reader kept the pipe open, the
    /// agent never noticed, and the attach at the end was refused forever.
    #[test]
    #[cfg(target_os = "windows")]
    fn a_link_that_is_let_go_frees_the_agent_for_the_next_one() {
        use pipes::*;

        within("letting go of an agent link and connecting again", || {
            let desktop = format!("TainTest-{}-free", std::process::id());
            let agent = FakeAgent::serve(&desktop, true);
            let slot = Mutex::new(Weak::new());

            let first = attach_within(&slot, &desktop);
            let second = AgentLink::attach(&slot, &desktop).expect("a live link to join");
            assert!(Arc::ptr_eq(&first, &second), "two sessions share one connection");

            let started = Instant::now();
            let (generation, frame) = loop {
                if let Some(arrived) = first.newer_than(0) {
                    break arrived;
                }
                assert!(started.elapsed() < Duration::from_secs(3), "the agent's frame never arrived");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!((frame.width, frame.height), (2, 2));
            assert!(
                second.newer_than(0).is_some(),
                "one session seeing a frame does not take it from another"
            );
            assert!(first.newer_than(generation).is_none(), "nobody is handed the same frame twice");
            assert_eq!(agent.connections(), 1);

            drop(first);
            drop(second);

            let third = attach_within(&slot, &desktop);
            assert_eq!(
                agent.connections(),
                2,
                "letting go closed the connection, and the agent served the next one"
            );
            drop(third);
        });
    }

    /// Input reaches the agent while the link is waiting for a frame.
    ///
    /// Which is most of the time: an agent sends only when its desktop
    /// changes. On a handle that is not overlapped Windows runs one operation
    /// at a time, so this click would have waited behind that read until the
    /// picture moved — and the message a closing link sends would have waited
    /// forever, which is how this was found.
    #[test]
    #[cfg(target_os = "windows")]
    fn input_reaches_the_agent_while_a_frame_is_awaited() {
        use crate::protocol::{InputEvent, MouseEvent};
        use pipes::*;

        within("sending input while waiting for a frame", || {
            let desktop = format!("TainTest-{}-input", std::process::id());
            // An agent that never sends a frame, so the read never finishes.
            let agent = FakeAgent::serve(&desktop, false);
            let slot = Mutex::new(Weak::new());

            let link = attach_within(&slot, &desktop);
            // Long enough that the reader is certainly inside its read.
            std::thread::sleep(Duration::from_millis(100));

            link.frames()
                .send_input(&InputEvent::MouseMove(MouseEvent { x: 1, y: 2 }))
                .expect("a live link accepts input");

            let started = Instant::now();
            while agent.inputs() == 0 {
                assert!(started.elapsed() < Duration::from_secs(3), "the click never reached the agent");
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(link);
        });
    }

    /// A stand-in agent on a real named pipe, and the helpers the tests share.
    #[cfg(target_os = "windows")]
    mod pipes {
        pub(super) use super::platform::AgentLink;
        use crate::agent_pipe::{serve_forever, PipeFrame, PipeRequest};
        use crate::screen_capture::PixelFormat;
        pub use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        pub use std::sync::{Arc, Mutex, Weak};
        pub use std::time::{Duration, Instant};

        /// Never cleared: the fake agents live until the test process ends.
        static RUNNING: AtomicBool = AtomicBool::new(true);

        /// Serves a pipe the way the agents serve theirs — one instance, and no
        /// way to learn that a host has left except through its input.
        pub struct FakeAgent {
            connections: Arc<AtomicUsize>,
            inputs: Arc<AtomicUsize>,
        }

        impl FakeAgent {
            /// `one_frame`: send a single frame when frames are wanted, then
            /// nothing — an agent whose desktop has just lost the input.
            pub fn serve(desktop: &str, one_frame: bool) -> Self {
                let connections = Arc::new(AtomicUsize::new(0));
                let inputs = Arc::new(AtomicUsize::new(0));
                let (desktop, counted, heard) =
                    (desktop.to_string(), Arc::clone(&connections), Arc::clone(&inputs));
                std::thread::spawn(move || {
                    let _ = serve_forever(&desktop, &RUNNING, |writer| {
                        counted.fetch_add(1, Ordering::Relaxed);
                        let mut sent = false;
                        loop {
                            for request in writer.take_input()? {
                                match request {
                                    PipeRequest::FramesWanted(true) if one_frame && !sent => {
                                        writer.send(&PipeFrame {
                                            width: 2,
                                            height: 2,
                                            format: PixelFormat::Bgra.to_wire(),
                                            data: vec![0; 16],
                                        })?;
                                        sent = true;
                                    }
                                    PipeRequest::Input(_) => {
                                        heard.fetch_add(1, Ordering::Relaxed);
                                    }
                                    _ => {}
                                }
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    });
                });
                Self { connections, inputs }
            }

            pub fn connections(&self) -> usize {
                self.connections.load(Ordering::Relaxed)
            }

            pub fn inputs(&self) -> usize {
                self.inputs.load(Ordering::Relaxed)
            }
        }

        /// Attach, retrying while the pipe is not there yet or still busy.
        pub fn attach_within(slot: &Mutex<Weak<AgentLink>>, desktop: &str) -> Arc<AgentLink> {
            let started = Instant::now();
            loop {
                match AgentLink::attach(slot, desktop) {
                    Ok(link) => return link,
                    Err(_) if started.elapsed() < Duration::from_secs(3) => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => panic!("the agent's pipe never came free: {e:#}"),
                }
            }
        }

        /// Run `body` on a thread and fail if it has not finished in time.
        ///
        /// These tests exist because something waited forever. One that waited
        /// forever itself would say nothing about why, and would hold the run
        /// up until somebody noticed — so a stall is a failure with a name.
        pub fn within(what: &str, body: impl FnOnce() + Send + 'static) {
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                body();
                let _ = done.send(());
            });
            match finished.recv_timeout(Duration::from_secs(20)) {
                Ok(()) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("{what} did not finish within 20 seconds — something is waiting forever")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{what} failed — see the panic above")
                }
            }
        }
    }

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
