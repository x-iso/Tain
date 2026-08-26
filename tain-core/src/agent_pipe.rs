//! Frames from an agent on another desktop, over a named pipe.
//!
//! # What this is for
//!
//! A process can only capture the desktop it was created on. When Windows
//! raises a consent prompt it switches the input to the secure desktop, and the
//! host — running as the signed-in user on `Default` — can no longer see
//! anything. Measured on the machine that reported it, the guest gets one dimmed
//! frame and then nothing, with the pointer frozen.
//!
//! The service already keeps an agent on the secure desktop, and that agent can
//! capture it: 2560x1600, first attempt, 99.9% of pixels not black — a real
//! consent prompt over a real dimmed desktop. So the missing piece is carrying
//! those frames the short distance from the agent to whoever is hosting.
//!
//! # Who serves and who connects
//!
//! The agent serves. It knows when it owns the input and it is the only thing
//! that can capture its own desktop, so it is where a frame exists first. The
//! host connects when it needs frames and disconnects when it does not, which
//! also decides when the agent captures at all — an idle agent holding a
//! duplication would cost a GPU resource for nothing, all day.
//!
//! # Who may connect
//!
//! Only the user signed in at the console, and Local System.
//!
//! This matters more here than anywhere else in the application. The pipe
//! carries pictures of the secure desktop, and once input is added it will
//! carry clicks for it too. A pipe that any authenticated user could open would
//! let one signed-in user watch and drive another's consent prompts, which is a
//! privilege boundary Windows spends considerable effort maintaining and not
//! one to give away with a default security descriptor.
//!
//! The console user is exactly right, and no wider: it is their session, their
//! screen, and their prompt. Seeing a consent dialog is not the same as
//! satisfying it — a standard user is still asked for administrator
//! credentials — so this grants no privilege that being at the keyboard would
//! not.

use anyhow::Result;

/// The pipe an agent on `desktop` serves.
///
/// Named per desktop rather than per session because there is one agent per
/// desktop and they must not collide. A session prefix would be needed for
/// multiple simultaneous sessions, which is a later problem and noted here so
/// it is not discovered as a mystery.
pub fn pipe_name(desktop: &str) -> String {
    let safe: String = desktop
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!(r"\\.\pipe\tain-agent-{safe}")
}

/// One frame, as it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeFrame {
    pub width: u16,
    pub height: u16,
    /// The pixel format, as `screen_capture::PixelFormat` encodes it.
    pub format: u8,
    pub data: Vec<u8>,
}

/// Bytes on the wire: a fixed header, then the pixels.
///
/// Deliberately not a serialisation library. There is exactly one message, both
/// ends ship together in the same executable, and a hand-written header of five
/// fields is easier to be sure about than a derive that could silently change
/// shape with a dependency bump.
pub const HEADER_LEN: usize = 12;

/// Guards against a corrupt or hostile length turning into an allocation.
///
/// Sized for the largest frame anyone will send through this: 8K at four bytes
/// a pixel is 132 MB, and this is comfortably above it while still being far
/// below "allocate whatever the other end says".
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Magic, so a wrong pipe or a desynchronised stream fails immediately rather
/// than being read as a plausible frame.
const MAGIC: u32 = 0x5441_494e; // "TAIN"

impl PipeFrame {
    /// The header for this frame.
    pub fn header(&self) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..6].copy_from_slice(&self.width.to_le_bytes());
        header[6..8].copy_from_slice(&self.height.to_le_bytes());
        header[8] = self.format;
        // One byte spare, and the length as three so the header stays twelve.
        let len = self.data.len() as u32;
        header[9..12].copy_from_slice(&len.to_le_bytes()[0..3]);
        header
    }

    /// Read a header back. `None` when it is not one of ours.
    pub fn parse_header(header: &[u8; HEADER_LEN]) -> Option<(u16, u16, u8, usize)> {
        let magic = u32::from_le_bytes(header[0..4].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        let width = u16::from_le_bytes(header[4..6].try_into().ok()?);
        let height = u16::from_le_bytes(header[6..8].try_into().ok()?);
        let format = header[8];
        let len =
            u32::from_le_bytes([header[9], header[10], header[11], 0]) as usize;
        Some((width, height, format, len))
    }
}

/// Whether `len` could plausibly be a frame of `width` by `height`.
///
/// A length that does not match its own dimensions means the stream is not
/// where it thinks it is, and reading it as pixels would hand a scrambled
/// picture to the guest rather than failing.
pub fn length_is_consistent(width: u16, height: u16, len: usize) -> bool {
    if len == 0 || len > MAX_FRAME_BYTES {
        return false;
    }
    let pixels = width as usize * height as usize;
    // Four bytes a pixel for BGRA, three halves for NV12. Anything else is not
    // a format this carries.
    len == pixels * 4 || len == pixels * 3 / 2
}

/// One input event, travelling the other way.
///
/// # A fixed eight bytes, not a length-prefixed message
///
/// Every event fits: the largest is a key, at six bytes of payload. A fixed
/// record means the reader can never be told to wait for bytes that are not
/// coming, which for the side running as SYSTEM on the secure desktop is worth
/// more than the two bytes it wastes on a mouse move.
pub const INPUT_LEN: usize = 8;

/// Turn an event into its eight bytes.
///
/// Coordinates are the normalised space [`crate::protocol::MouseEvent`] already
/// uses, so nothing here has to know the size of a screen — which matters,
/// because the desktop the event is bound for may not be the one the guest was
/// looking at when it was made.
pub fn encode_input(event: &crate::protocol::InputEvent) -> [u8; INPUT_LEN] {
    use crate::protocol::InputEvent;

    let mut out = [0u8; INPUT_LEN];
    match event {
        InputEvent::MouseMove(m) => {
            out[0] = 1;
            out[1..3].copy_from_slice(&m.x.to_le_bytes());
            out[3..5].copy_from_slice(&m.y.to_le_bytes());
        }
        InputEvent::MouseButton(b) => {
            out[0] = 2;
            out[1] = b.button;
            out[2] = b.pressed as u8;
        }
        InputEvent::Scroll(s) => {
            out[0] = 3;
            out[1..3].copy_from_slice(&s.dx.to_le_bytes());
            out[3..5].copy_from_slice(&s.dy.to_le_bytes());
        }
        InputEvent::Key(k) => {
            out[0] = 4;
            out[1..3].copy_from_slice(&k.keycode.to_le_bytes());
            out[3..5].copy_from_slice(&k.scancode.to_le_bytes());
            out[5] = k.pressed as u8;
            out[6] = k.extended as u8;
        }
    }
    out
}

/// Ask the agent to start or stop capturing, in the same eight bytes.
///
/// A link opened only to deliver input — which is what an elevated window in
/// the foreground needs — has no use for frames, and an agent duplicating a
/// desktop nobody is watching holds a GPU resource for nothing.
pub fn encode_frames_wanted(wanted: bool) -> [u8; INPUT_LEN] {
    let mut out = [0u8; INPUT_LEN];
    out[0] = 5;
    out[1] = wanted as u8;
    out
}

/// What a record turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeRequest {
    /// Inject this on the agent's desktop.
    Input(crate::protocol::InputEvent),
    /// Start or stop capturing.
    FramesWanted(bool),
}

/// Read one record, whatever kind it is.
pub fn decode_request(bytes: &[u8; INPUT_LEN]) -> Option<PipeRequest> {
    if bytes[0] == 5 {
        return Some(PipeRequest::FramesWanted(bytes[1] != 0));
    }
    decode_input(bytes).map(PipeRequest::Input)
}

/// Read one back. `None` for a record this version does not know.
pub fn decode_input(bytes: &[u8; INPUT_LEN]) -> Option<crate::protocol::InputEvent> {
    use crate::protocol::{
        InputEvent, KeyEvent, MouseButtonEvent, MouseEvent, ScrollEvent,
    };

    let pair = |a: usize| u16::from_le_bytes([bytes[a], bytes[a + 1]]);
    let signed = |a: usize| i16::from_le_bytes([bytes[a], bytes[a + 1]]);

    Some(match bytes[0] {
        1 => InputEvent::MouseMove(MouseEvent { x: pair(1), y: pair(3) }),
        2 => InputEvent::MouseButton(MouseButtonEvent {
            button: bytes[1],
            pressed: bytes[2] != 0,
        }),
        3 => InputEvent::Scroll(ScrollEvent { dx: signed(1), dy: signed(3) }),
        4 => InputEvent::Key(KeyEvent {
            keycode: pair(1),
            pressed: bytes[5] != 0,
            modifiers: 0,
            scancode: pair(3),
            extended: bytes[6] != 0,
        }),
        // An event kind added by a newer build on the other side. Skipping one
        // beats refusing the whole stream: the connection stays up and the
        // events both ends do understand keep working.
        _ => return None,
    })
}

#[cfg(target_os = "windows")]
pub use platform::{connect, serve_forever, AgentFrames, PipeWriter};

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};

    use crate::pipe_security::{console_user_sid, wide, OwnedHandle as Owned, PipeSecurity};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// One connected host: frames out, input in.
    ///
    /// Handed to the session callback, which owns the loop. Input is drained
    /// rather than waited on — see [`Self::take_input`].
    pub struct PipeWriter {
        pipe: HANDLE,
    }

    impl PipeWriter {
        /// Every input event the host has sent since the last call.
        ///
        /// # Why this peeks rather than reads
        ///
        /// A read on a pipe blocks until something arrives, and this is called
        /// from the same loop that captures. Blocking here would mean the
        /// picture stops whenever the guest stops moving the mouse, which is
        /// most of the time somebody is looking at a consent prompt deciding
        /// what to do. Asking how much is waiting first costs one call and
        /// cannot block at all.
        ///
        /// Partial records are left in the pipe. Input arrives in whole
        /// eight-byte units and a short read would mean the next event is
        /// assembled from the tail of one and the head of another — a click at
        /// coordinates nobody asked for.
        pub fn take_input(&mut self) -> Vec<PipeRequest> {
            let mut events = Vec::new();
            loop {
                let mut available = 0u32;
                // SAFETY: a live pipe handle; every out parameter is a local.
                let peeked = unsafe {
                    PeekNamedPipe(self.pipe, None, 0, None, Some(&mut available), None)
                };
                if peeked.is_err() || (available as usize) < INPUT_LEN {
                    return events;
                }

                let mut record = [0u8; INPUT_LEN];
                let mut read = 0u32;
                // SAFETY: as above, into a local buffer.
                let got = unsafe {
                    ReadFile(self.pipe, Some(&mut record), Some(&mut read), None)
                };
                if got.is_err() || read as usize != INPUT_LEN {
                    return events;
                }
                // An unknown kind is skipped, not fatal. See `decode_input`.
                if let Some(request) = decode_request(&record) {
                    events.push(request);
                }
            }
        }
        /// Send one frame. An error means the host has gone.
        pub fn send(&mut self, frame: &PipeFrame) -> Result<()> {
            self.write_all(&frame.header())?;
            self.write_all(&frame.data)
        }

        /// Named pipes are free to accept less than offered, so this loops.
        fn write_all(&mut self, mut bytes: &[u8]) -> Result<()> {
            while !bytes.is_empty() {
                let mut wrote = 0u32;
                // SAFETY: a live pipe handle and a slice that outlives the call.
                unsafe { WriteFile(self.pipe, Some(bytes), Some(&mut wrote), None) }
                    .context("the host stopped reading")?;
                if wrote == 0 {
                    bail!("the host stopped reading");
                }
                bytes = &bytes[wrote as usize..];
            }
            Ok(())
        }
    }

    /// Serve this desktop's pipe until `running` goes false.
    ///
    /// One client at a time, which is all there can be: a host either wants the
    /// secure desktop or it does not. `session` is called once per connection
    /// and returns when the host disconnects, so it is also the signal to start
    /// and stop capturing — an agent that duplicated its desktop all day would
    /// hold a GPU resource for nothing.
    pub fn serve_forever(
        desktop: &str,
        running: &AtomicBool,
        mut session: impl FnMut(&mut PipeWriter) -> Result<()>,
    ) -> Result<()> {
        let name = wide(&pipe_name(desktop));
        let sid = console_user_sid()?;

        while running.load(Ordering::Relaxed) {
            let security = PipeSecurity::for_console_user(&sid)?;

            // SAFETY: the name and attributes outlive the call.
            let pipe = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    // Room for a burst without a syscall per row, and no more.
                    1 << 20,
                    1 << 16,
                    0,
                    Some(security.attributes()),
                )
            };
            // `CreateNamedPipeW` reports failure with a sentinel rather than
            // a `Result`, so it is checked rather than unwrapped.
            if pipe.is_invalid() {
                bail!(
                    "could not create the agent pipe: {}",
                    windows::core::Error::from_win32()
                );
            }
            let pipe = Owned(pipe);

            // SAFETY: a pipe this function owns and has not connected yet.
            let connected = unsafe { ConnectNamedPipe(pipe.0, None) };
            if let Err(e) = &connected
                && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
            {
                // A host that connected between create and connect is already
                // here, which is a success wearing an error's clothes.
                tracing::debug!("Agent pipe on {desktop}: waiting for a host: {e}");
                continue;
            }

            tracing::info!("Agent pipe on {desktop}: a host connected");
            let mut writer = PipeWriter { pipe: pipe.0 };
            if let Err(e) = session(&mut writer) {
                tracing::info!("Agent pipe on {desktop}: session ended: {e:#}");
            }
            // SAFETY: connected immediately above.
            unsafe {
                let _ = DisconnectNamedPipe(pipe.0);
            }
        }
        Ok(())
    }

    /// Frames arriving from an agent on another desktop, and input going back.
    pub struct AgentFrames {
        pipe: Owned,
        /// Held for the length of one input record. See [`Self::send_input`].
        writing: std::sync::Mutex<()>,
        /// Held for the length of one frame. See [`Self::next_frame`].
        reading: std::sync::Mutex<()>,
    }

    // SAFETY: the handle inside is a kernel object, not a pointer into this
    // process, and nothing here caches thread state. Reading and writing happen
    // on different threads, which a duplex pipe supports directly, and the
    // write side is serialised by the mutex below.
    unsafe impl Send for AgentFrames {}
    // SAFETY: as above. `send_input` takes `&self` and holds `writing` for the
    // whole of each record, so two threads cannot interleave halves of one.
    unsafe impl Sync for AgentFrames {}

    impl AgentFrames {
        /// Send one input event to the agent, to inject on its own desktop.
        ///
        /// The lock covers a whole record. Half of one event followed by half
        /// of another is not a dropped click, it is a click somewhere else.
        pub fn send_input(&self, event: &crate::protocol::InputEvent) -> Result<()> {
            self.send_record(encode_input(event))
        }

        /// Tell the agent whether to capture at all.
        pub fn send_frames_wanted(&self, wanted: bool) -> Result<()> {
            self.send_record(encode_frames_wanted(wanted))
        }

        fn send_record(&self, record: [u8; INPUT_LEN]) -> Result<()> {
            let _serialised = self.writing.lock().map_err(|_| {
                anyhow::anyhow!("the input channel to the agent was poisoned")
            })?;

            let mut sent = 0usize;
            while sent < record.len() {
                let mut wrote = 0u32;
                // SAFETY: a live pipe handle and a slice that outlives the call.
                unsafe {
                    WriteFile(self.pipe.0, Some(&record[sent..]), Some(&mut wrote), None)
                }
                .context("the agent stopped accepting input")?;
                if wrote == 0 {
                    bail!("the agent stopped accepting input");
                }
                sent += wrote as usize;
            }
            Ok(())
        }
    }

    /// Connect to the agent serving `desktop`.
    pub fn connect(desktop: &str) -> Result<AgentFrames> {
        let name = wide(&pipe_name(desktop));
        // SAFETY: the name outlives the call.
        let pipe = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                // Both ways: frames come back, input goes out.
                windows::Win32::Foundation::GENERIC_READ.0
                    | windows::Win32::Foundation::GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .with_context(|| format!("no agent is serving {desktop}"))?;
        Ok(AgentFrames {
            pipe: Owned(pipe),
            writing: std::sync::Mutex::new(()),
            reading: std::sync::Mutex::new(()),
        })
    }

    impl AgentFrames {
        /// Read the next frame. Blocks until one arrives or the agent goes.
        ///
        /// Takes `&self` because the link is shared with the input path — see
        /// `capture_follow`. Only one thread ever reads, and the guard says so
        /// rather than leaving it to be true by accident.
        pub fn next_frame(&self) -> Result<PipeFrame> {
            let _serialised = self
                .reading
                .lock()
                .map_err(|_| anyhow::anyhow!("the frame channel from the agent was poisoned"))?;
            let mut header = [0u8; HEADER_LEN];
            self.read_exact(&mut header)?;
            let (width, height, format, len) =
                PipeFrame::parse_header(&header).context("the agent sent something else")?;

            // Checked before allocating, not after. The length arrives from
            // another process, and "trust it and allocate" is how a corrupt
            // header becomes an out-of-memory abort.
            if !length_is_consistent(width, height, len) {
                bail!("the agent sent a {width}x{height} frame of {len} bytes, which is not one");
            }

            let mut data = vec![0u8; len];
            self.read_exact(&mut data)?;
            Ok(PipeFrame { width, height, format, data })
        }

        fn read_exact(&self, mut into: &mut [u8]) -> Result<()> {
            while !into.is_empty() {
                let mut read = 0u32;
                // SAFETY: a live pipe handle and a slice that outlives the call.
                unsafe { ReadFile(self.pipe.0, Some(into), Some(&mut read), None) }
                    .context("the agent stopped sending")?;
                if read == 0 {
                    bail!("the agent closed the pipe");
                }
                into = &mut into[read as usize..];
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn serve(_desktop: &str, _frames: impl FnMut() -> Option<PipeFrame>) -> Result<()> {
    anyhow::bail!("agent pipes are Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event shape survives the trip, including the signs and the flags
    /// that are easy to drop.
    #[test]
    fn every_input_event_survives_the_round_trip() {
        use crate::protocol::{
            InputEvent, KeyEvent, MouseButtonEvent, MouseEvent, ScrollEvent,
        };

        let events = [
            InputEvent::MouseMove(MouseEvent { x: 40000, y: 65535 }),
            InputEvent::MouseButton(MouseButtonEvent { button: 2, pressed: true }),
            InputEvent::MouseButton(MouseButtonEvent { button: 0, pressed: false }),
            InputEvent::Scroll(ScrollEvent { dx: -120, dy: 240 }),
            InputEvent::Key(KeyEvent {
                keycode: 0x5B,
                pressed: true,
                modifiers: 0,
                scancode: 0x5B,
                extended: true,
            }),
        ];

        for event in events {
            let decoded = decode_input(&encode_input(&event)).expect("our own encoding");
            assert_eq!(decoded, event, "round trip changed the event");
        }
    }

    /// The two record kinds share eight bytes and must not be read as each
    /// other.
    #[test]
    fn a_frames_request_is_not_mistaken_for_input() {
        use crate::protocol::{InputEvent, MouseButtonEvent};

        for wanted in [true, false] {
            assert_eq!(
                decode_request(&encode_frames_wanted(wanted)),
                Some(PipeRequest::FramesWanted(wanted)),
            );
        }

        // And an input event still reads as one. A left click is the record
        // most likely to collide by accident: it is nearly all zeroes.
        let click = InputEvent::MouseButton(MouseButtonEvent { button: 0, pressed: true });
        assert_eq!(
            decode_request(&encode_input(&click)),
            Some(PipeRequest::Input(click)),
        );
    }

    /// A newer build sending something this one does not know must not take the
    /// connection down with it.
    #[test]
    fn an_unknown_event_kind_is_skipped_rather_than_fatal() {
        let mut bytes = [0u8; INPUT_LEN];
        bytes[0] = 200;
        assert!(decode_input(&bytes).is_none());
    }

    #[test]
    fn a_header_survives_the_round_trip() {
        let frame = PipeFrame { width: 2560, height: 1600, format: 1, data: vec![7; 64] };
        let parsed = PipeFrame::parse_header(&frame.header()).expect("our own header");
        assert_eq!(parsed, (2560, 1600, 1, 64));
    }

    #[test]
    fn a_foreign_header_is_refused() {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert!(PipeFrame::parse_header(&header).is_none());
    }

    /// The pipe name is built from a desktop name, which comes from the system
    /// rather than from us.
    #[test]
    fn a_desktop_name_cannot_escape_the_pipe_namespace() {
        assert_eq!(pipe_name(r"..\..\evil"), r"\\.\pipe\tain-agent-evil");
        assert_eq!(pipe_name("Winlogon"), r"\\.\pipe\tain-agent-Winlogon");
    }

    #[test]
    fn a_length_that_does_not_match_its_dimensions_is_refused() {
        // BGRA and NV12 for 640x480 are both accepted.
        assert!(length_is_consistent(640, 480, 640 * 480 * 4));
        assert!(length_is_consistent(640, 480, 640 * 480 * 3 / 2));
        // Anything else is a stream that has lost its place.
        assert!(!length_is_consistent(640, 480, 640 * 480 * 4 - 1));
        assert!(!length_is_consistent(640, 480, 0));
        assert!(!length_is_consistent(640, 480, MAX_FRAME_BYTES + 1));
    }

    /// A frame claiming to be enormous must be refused before it is allocated.
    #[test]
    fn an_absurd_length_is_refused_rather_than_allocated() {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..6].copy_from_slice(&8u16.to_le_bytes());
        header[6..8].copy_from_slice(&8u16.to_le_bytes());
        header[9..12].copy_from_slice(&0xFF_FFFFu32.to_le_bytes()[0..3]);
        let (w, h, _, len) = PipeFrame::parse_header(&header).expect("our own header");
        assert!(!length_is_consistent(w, h, len));
    }
}
