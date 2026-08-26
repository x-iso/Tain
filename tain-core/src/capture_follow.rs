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
pub use platform::{send_input_to_agent, DesktopFollowing};

/// Whether input should be going to an agent rather than injected locally.
#[cfg(not(target_os = "windows"))]
pub fn send_input_to_agent(_event: &crate::protocol::InputEvent) -> bool {
    false
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

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

    /// Send an event to the agent, if one is currently supplying frames.
    ///
    /// Returns whether it went. `false` means inject locally as usual — there
    /// is no agent involved, which is the ordinary case.
    pub fn send_input_to_agent(event: &crate::protocol::InputEvent) -> bool {
        let Ok(link) = LINK.lock() else { return false };
        let Some(agent) = link.as_ref() else { return false };
        match agent.send_input(event) {
            Ok(()) => true,
            Err(e) => {
                // Not retried and not fatal. The capturer notices the same
                // failure on its next frame and drops the link; until then,
                // losing a click beats injecting it on the wrong desktop.
                tracing::debug!("Could not send input to the agent: {e:#}");
                false
            }
        }
    }

    /// Wraps a local capturer and falls back to an agent's frames.
    pub struct DesktopFollowing {
        local: Box<dyn ScreenCaptureBackend>,
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
            Self { local, agent: None, had_local_input: true, from_agent: 0 }
        }

        /// Whether this process can still see the desktop receiving input.
        ///
        /// `OpenInputDesktop` refusing is the signal, not an error: a process on
        /// the ordinary desktop is not permitted to open the secure one, so
        /// being turned away is precisely how it learns the secure desktop is
        /// up. There is no positive notification of this anywhere in Windows.
        fn local_has_input() -> bool {
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

            if local {
                return self.local.capture_next();
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
            self.had_local_input.then(|| self.local.take_cursor()).flatten()
        }

        fn frame_size(&self) -> (u16, u16) {
            self.local.frame_size()
        }

        fn set_scale(&mut self, scale: QualityScale) -> bool {
            self.local.set_scale(scale)
        }

        fn request_format(&mut self, format: PixelFormat) -> bool {
            // Agent frames arrive as the agent captured them, which is BGRA.
            // Accepting a negotiated NV12 here would be a promise this cannot
            // keep the moment a consent prompt appears.
            if format != PixelFormat::Bgra {
                return false;
            }
            self.local.request_format(format)
        }

        fn is_real(&self) -> bool {
            self.local.is_real()
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
