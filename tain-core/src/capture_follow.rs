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

/// The desktop a signed-in user's session lives on.
///
/// Uninteresting to a process that has one of its own — it is simply where it
/// already is. It matters to the service, which has no desktop at all and must
/// name this one to be handed its frames.
pub const ORDINARY_DESKTOP: &str = "Default";

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
        /// Which agent is being asked, as an index into [`AGENT_DESKTOPS`].
        ///
        /// Only moves in agent-only mode. A process with a desktop of its own
        /// asks the secure desktop or nothing, because the only reason it ever
        /// asks is that the input went somewhere it cannot see.
        asking: usize,
        /// When the current agent last had something to say.
        ///
        /// Silence is the whole signal here. See [`Self::rotate_if_silent`].
        heard: std::time::Instant,
    }

    /// The desktops an agent-only capturer will ask, in order.
    ///
    /// Ordinary first: a machine with somebody signed in is the common case and
    /// settling on the first try saves a rotation. With nobody signed in the
    /// ordinary desktop has no input, says nothing, and the secure desktop —
    /// where the sign-in screen is — answers on the next pass.
    const AGENT_DESKTOPS: [&str; 2] = [ORDINARY_DESKTOP, SECURE_DESKTOP];

    /// How long an agent may say nothing before another is tried.
    ///
    /// This is not a timeout on a slow desktop. An agent that owns the input
    /// sends frames whether or not the picture changed, so silence this long
    /// means it does not own the input rather than that nothing is happening.
    /// Long enough not to rotate across the moment a desktop switch is
    /// half-done, short enough that a guest waits about a second.
    const SILENCE_BEFORE_ROTATING: std::time::Duration = std::time::Duration::from_millis(1000);

    impl DesktopFollowing {
        pub fn new(local: Box<dyn ScreenCaptureBackend>) -> Self {
            Self {
                local: Some(local),
                agent: None,
                had_local_input: true,
                from_agent: 0,
                // The secure desktop, and never moved: see `asking`.
                asking: 1,
                heard: std::time::Instant::now(),
            }
        }

        /// A capturer with no desktop of its own, for the service.
        ///
        /// Every frame comes from an agent. Which agent is not a constant, and
        /// assuming it was is what made this show the wrong thing: it always
        /// asked the secure desktop, which is right at the sign-in screen and
        /// wrong the moment somebody is signed in with no application running —
        /// then the input is on the ordinary desktop, the secure desktop's
        /// agent owns nothing and sends nothing, and a guest connects to a
        /// picture that never arrives.
        ///
        /// So it asks each in turn and settles on whichever answers. There is
        /// no need to work out which desktop has the input, which is fortunate:
        /// session zero cannot. `OpenInputDesktop` answers for the caller's own
        /// window station, and the service is not in one that can see these.
        /// The agents can each tell, and the one that can is the one that
        /// speaks.
        pub fn agent_only() -> Self {
            Self {
                local: None,
                agent: None,
                had_local_input: false,
                from_agent: 0,
                asking: 0,
                heard: std::time::Instant::now(),
            }
        }

        /// The desktop currently being asked for frames.
        fn asking(&self) -> &'static str {
            AGENT_DESKTOPS[self.asking % AGENT_DESKTOPS.len()]
        }

        /// Move to the next agent when this one has gone quiet.
        ///
        /// Dropping the reader closes the pipe, which stops that agent
        /// capturing, and clears [`LINK`] so no click is sent down a pipe that
        /// is about to close — the same order as a desktop switch, for the same
        /// reason.
        ///
        /// Only in agent-only mode. A process with a desktop of its own is
        /// asking an agent precisely because the input went to the one desktop
        /// it cannot see, so there is no second candidate to try — and rotating
        /// onto the ordinary desktop would have it ask an agent for the very
        /// desktop it is standing on.
        fn rotate_if_silent(&mut self) {
            if !capture_through_agent_only() {
                return;
            }
            if self.heard.elapsed() < SILENCE_BEFORE_ROTATING {
                return;
            }
            let was = self.asking();
            self.asking += 1;
            self.heard = std::time::Instant::now();
            if let Ok(mut link) = LINK.lock() {
                *link = None;
            }
            self.agent = None;
            tracing::debug!(
                "The {was} agent has nothing to show — it does not have the input, so \
                 asking {} instead",
                self.asking(),
            );
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
                         asking the {} agent for frames",
                        self.asking(),
                    );
                    // A fresh look, so the silence that decides whether to try
                    // another agent is measured from here and not from whenever
                    // this capturer happened to be built.
                    self.heard = std::time::Instant::now();
                }
            }

            if local && let Some(local) = self.local.as_mut() {
                return local.capture_next();
            }

            let desktop = self.asking();

            // Connecting is retried on each call rather than once, because the
            // agent may not have its pipe up at the instant the desktop
            // switches, and the alternative is giving up for the whole prompt.
            if self.agent.is_none() {
                match AgentReader::start(desktop) {
                    Ok(reader) => self.agent = Some(reader),
                    Err(e) => {
                        tracing::debug!("No frames from the {desktop} agent yet: {e:#}");
                        // An agent that is not there at all is as good as one
                        // with nothing to say, and on a machine where only one
                        // desktop has an agent this is how the other is found.
                        self.rotate_if_silent();
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
                    self.heard = std::time::Instant::now();
                    self.from_agent += 1;
                    if self.from_agent == 1 {
                        tracing::info!(
                            "First frame from the {desktop} agent: {}x{}",
                            frame.width,
                            frame.height,
                        );
                    }
                    Ok(Some(frame))
                }
                None if reader.finished() => {
                    tracing::info!("The {desktop} agent stopped sending");
                    if let Ok(mut link) = LINK.lock() {
                        *link = None;
                    }
                    self.agent = None;
                    Ok(None)
                }
                // Connected and quiet. Either this desktop has nothing new to
                // show, or it does not have the input and never will have
                // anything to show; only time tells those apart.
                None => {
                    self.rotate_if_silent();
                    Ok(None)
                }
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
        /// Nothing to forward to when the agent is doing the capturing. It
        /// sends what it sends, and the size follows the desktop back.
        fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
            self.local.as_mut().is_some_and(|local| local.set_size_ceiling(ceiling))
        }

        /// Same delegation, and the same nothing to delegate to when the agent
        /// is capturing — an agent frame is whatever size the desktop is.
        fn set_size_granularity(&mut self, granularity: (u16, u16)) -> bool {
            self.local.as_mut().is_some_and(|local| local.set_size_granularity(granularity))
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
