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

#[cfg(target_os = "windows")]
pub use platform::{connect, serve_forever, AgentFrames, PipeWriter};

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Security::{
        Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// Null-terminated UTF-16, which is what every call here wants.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes a handle however the surrounding code exits.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: owned by this type, closed once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// The SID of the user this pipe is for, as a string.
    ///
    /// An agent on the secure desktop runs as SYSTEM and has to ask which user
    /// is at the console. One on the ordinary desktop already *is* that user, so
    /// it reads its own token — asking `WTSQueryUserToken` there would fail, and
    /// the answer is the same.
    fn console_user_sid() -> Result<String> {
        use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows::Win32::System::RemoteDesktop::{
            WTSGetActiveConsoleSessionId, WTSQueryUserToken,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SAFETY: a documented read of global session state.
        let session = unsafe { WTSGetActiveConsoleSessionId() };

        // SAFETY: both paths produce a token this function then owns.
        let token = unsafe {
            let mut token = HANDLE::default();
            if WTSQueryUserToken(session, &mut token).is_ok() {
                Owned(token)
            } else {
                let mut own = HANDLE::default();
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own)
                    .context("could not open this process's token")?;
                Owned(own)
            }
        };

        // SAFETY: a two-call query into a buffer sized by the first call.
        unsafe {
            let mut needed = 0u32;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .context("could not read the user out of the token")?;

            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut text = windows::core::PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut text)
                .context("could not render the user's SID")?;
            let owned = text.to_string().context("the SID was not valid text")?;
            let _ = windows::Win32::Foundation::LocalFree(
                windows::Win32::Foundation::HLOCAL(text.0 as *mut _),
            );
            Ok(owned)
        }
    }

    /// A security descriptor letting only Local System and the console user in.
    ///
    /// `D:P` makes it protected, so nothing is inherited from anywhere: this is
    /// the whole access list and not a starting point.
    fn pipe_security(user_sid: &str) -> Result<(PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES)> {
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GRGW;;;{user_sid})"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the string outlives the call; the descriptor is freed by the
        // caller once the pipe that uses it is created.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("could not build the pipe's access list")?;

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        Ok((descriptor, attributes))
    }

    /// Writes frames to a connected host. Handed to the session callback.
    pub struct PipeWriter {
        pipe: HANDLE,
    }

    impl PipeWriter {
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
            let (descriptor, attributes) = pipe_security(&sid)?;

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
                    Some(&attributes),
                )
            };
            // SAFETY: allocated by the conversion above, freed once here.
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(
                    windows::Win32::Foundation::HLOCAL(descriptor.0),
                );
            }
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

    /// Frames arriving from an agent on another desktop.
    pub struct AgentFrames {
        pipe: Owned,
    }

    // SAFETY: the handle inside is a kernel object, not a pointer into this
    // process, and nothing here caches thread state. It is moved to a reader
    // thread once and used only from there — see `capture_follow`, which reads
    // frames off the capture thread so a still consent prompt cannot stall the
    // session.
    unsafe impl Send for AgentFrames {}

    /// Connect to the agent serving `desktop`.
    pub fn connect(desktop: &str) -> Result<AgentFrames> {
        let name = wide(&pipe_name(desktop));
        // SAFETY: the name outlives the call.
        let pipe = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                windows::Win32::Foundation::GENERIC_READ.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .with_context(|| format!("no agent is serving {desktop}"))?;
        Ok(AgentFrames { pipe: Owned(pipe) })
    }

    impl AgentFrames {
        /// Read the next frame. Blocks until one arrives or the agent goes.
        pub fn next_frame(&mut self) -> Result<PipeFrame> {
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

        fn read_exact(&mut self, mut into: &mut [u8]) -> Result<()> {
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
