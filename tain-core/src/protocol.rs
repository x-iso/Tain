//! Protocol message types for the remote desktop connection.
//!
//! All messages are serialized with `bincode` and sent over iroh QUIC
//! bidirectional streams within a single ALPN-identified connection.

use serde::{Deserialize, Serialize};

// ── Frame data ────────────────────────────────────────────────

/// A video frame message carrying either encoded H.264 or raw BGRA pixels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMessage {
    /// Monotonically increasing sequence number (guest uses this to detect drops).
    pub sequence_number: u32,
    /// Display width in pixels.
    pub width: u16,
    /// Display height in pixels.
    pub height: u16,
    /// Timestamp in milliseconds since session start.
    pub timestamp_ms: u64,
    /// True if this is a keyframe (IDR / intra-only frame).
    pub is_keyframe: bool,
    /// The raw frame payload:
    /// - When `encoder = H264`, this contains one or more NAL units.
    /// - When `encoder = RawBgra`, this is `width * height * 4` bytes in B-G-R-A order.
    pub data: Vec<u8>,
}

// ── Input events (guest → host) ────────────────────────

/// Absolute pointer position, **normalized** to the host's full screen.
///
/// `0` is the left/top edge, [`ABSOLUTE_MAX`] the right/bottom edge. Normalizing
/// on the wire decouples input from the capture resolution — changing capture
/// size, or scaling the guest's window, cannot desync the pointer — and it maps
/// directly onto the Windows `MOUSEEVENTF_ABSOLUTE` range with no conversion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MouseEvent {
    pub x: u16,
    pub y: u16,
}

/// Upper bound of the normalized coordinate space used by [`MouseEvent`].
pub const ABSOLUTE_MAX: u16 = u16::MAX;

impl MouseEvent {
    /// Build from a fraction of the way across the screen, clamped to range.
    pub fn from_fraction(fx: f32, fy: f32) -> Self {
        let to_abs = |f: f32| (f.clamp(0.0, 1.0) * ABSOLUTE_MAX as f32).round() as u16;
        Self { x: to_abs(fx), y: to_abs(fy) }
    }
}

/// Mouse button press or release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseButtonEvent {
    /// Which mouse button (0=left, 1=right, 2=middle).
    pub button: u8,
    /// True for press, false for release.
    pub pressed: bool,
}

/// Mouse wheel movement, in **wheel-delta units** (120 per detent).
///
/// 120 is the Windows `WHEEL_DELTA` convention; using it on the wire means the
/// injector passes the value straight through, and high-resolution wheels can
/// send fractions of a detent without a separate protocol.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScrollEvent {
    /// Horizontal delta (positive = right).
    pub dx: i16,
    /// Vertical delta (positive = away from the user, i.e. scroll up).
    pub dy: i16,
}

/// Wheel units per detent, matching the Windows `WHEEL_DELTA` constant.
pub const WHEEL_DELTA: i16 = 120;

/// Keyboard key press or release.
///
/// `keycode` is always a **Windows virtual-key code**, on every platform. Picking
/// one canonical space keeps the translation on the injector side, where the OS
/// is already known, instead of making every guest guess what the host expects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KeyEvent {
    pub keycode: u16,
    /// True for press, false for release.
    pub pressed: bool,
    /// Modifier state when the event fired (Ctrl=1, Alt=2, Shift=4, Win/Super=8).
    ///
    /// Advisory only — modifier keys are also sent as their own `KeyEvent`s, so
    /// the host reconstructs modifier state from the key stream rather than
    /// trusting this. Useful for logging and for guests that cannot send
    /// standalone modifier events.
    pub modifiers: u8,
    /// The hardware scancode, when the sender had one. `0` means it did not.
    ///
    /// # Why this is preferred over the virtual key
    ///
    /// A virtual key is what Windows *decided* a keystroke meant, after
    /// applying the keyboard layout. Injecting one on the far machine asks that
    /// machine to reverse the decision using *its* layout, and the two do not
    /// have to agree — which is why letters survive the round trip and
    /// modifiers, system keys and anything layout-sensitive do not.
    ///
    /// A scancode is what the hardware sent: a position on the keyboard, before
    /// any interpretation. Injected with `KEYEVENTF_SCANCODE` it reproduces the
    /// same physical key regardless of either side's layout. This is the
    /// approach RustDesk's "Map" mode takes, and it is the one that works.
    ///
    /// The virtual key is still sent, because senders that have no scancode —
    /// the egui path, synthesised modifier releases — cannot produce one, and
    /// because it is what the logs are readable in.
    pub scancode: u16,
    /// Whether this key carries the extended-key prefix (`E0`).
    ///
    /// Part of the scancode's identity rather than a detail: without it, right
    /// Alt is left Alt, the arrow keys are the numpad, and Insert is Num0. The
    /// hook reports it separately from the scancode itself, so it travels
    /// separately too.
    pub extended: bool,
}

/// Bit values for [`KeyEvent::modifiers`].
pub mod modifier {
    pub const CTRL: u8 = 1;
    pub const ALT: u8 = 2;
    pub const SHIFT: u8 = 4;
    pub const SUPER: u8 = 8;
}

/// Composite input event message sent from guest to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMove(MouseEvent),
    MouseButton(MouseButtonEvent),
    Scroll(ScrollEvent),
    Key(KeyEvent),
}

impl InputEvent {
    /// Which device this event needs permission for.
    pub fn device(&self) -> InputDevice {
        match self {
            Self::MouseMove(_) | Self::MouseButton(_) | Self::Scroll(_) => InputDevice::Mouse,
            Self::Key(_) => InputDevice::Keyboard,
        }
    }
}

/// Input device class, used to gate events against [`ControlGrants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputDevice {
    Mouse,
    Keyboard,
}

/// Which input devices a session is permitted to drive.
///
/// Mouse and keyboard are tracked separately so a host can hand over pointer
/// control without also granting the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ControlGrants {
    pub mouse: bool,
    pub keyboard: bool,
}

impl ControlGrants {
    /// View-only: no input accepted.
    pub const NONE: Self = Self { mouse: false, keyboard: false };
    /// Full control.
    pub const ALL: Self = Self { mouse: true, keyboard: true };

    pub fn new(mouse: bool, keyboard: bool) -> Self {
        Self { mouse, keyboard }
    }

    /// Whether events from `device` may be injected.
    pub fn allows(&self, device: InputDevice) -> bool {
        match device {
            InputDevice::Mouse => self.mouse,
            InputDevice::Keyboard => self.keyboard,
        }
    }

    /// True if any device is permitted.
    pub fn any(&self) -> bool {
        self.mouse || self.keyboard
    }
}

// ── Control / session messages ────────────────────────────────

/// Resolution and quality configuration sent from guest to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Target frame rate (0 = let host decide).
    pub target_fps: u32,
    /// Maximum video bitrate in kbps (0 = unlimited / auto).
    pub max_bitrate_kbps: u32,
    /// Client display resolution for coordinate scaling.
    pub client_width: u16,
    pub client_height: u16,
}

/// Host-to-guest: this connection is waiting for the host to allow it.
///
/// Sent **before** [`SessionStart`], because nothing is streamed until the host
/// says yes — the guest sees this and knows to wait rather than assuming the
/// connection stalled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestApprovalRequest {
    /// Unique session identifier for this connection.
    pub session_id: u32,
    /// Human-friendly label the host can show (e.g. "Guest #1").
    pub label: String,
}

/// Longest device name accepted from a peer.
///
/// Names arrive from the other machine and go straight into the UI, so they are
/// bounded and stripped rather than trusted.
pub const MAX_DEVICE_NAME: usize = 40;

/// Clean a peer-supplied device name, or `None` if nothing usable is left.
///
/// Control characters would let a remote peer smuggle newlines into a label; the
/// length cap stops a 4 MB "name" from stretching the card off the screen.
pub fn sanitize_device_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME)
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Host → guest: the session's input permissions changed.
///
/// Sent whenever grants are set, so the guest can reflect the real state (and
/// stop sending input it knows will be dropped) instead of guessing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlStateMessage {
    pub session_id: u32,
    pub grants: ControlGrants,
}

// ── Telemetry ────────────────────────────────────────────────

/// Guest → host: how the far end of the pipeline is actually doing.
///
/// None of this is visible from the host. Decode cost, draw cost and how many
/// frames really made it onto a screen are facts about the other machine, and
/// without them the host cannot tell "the link is slow" from "their laptop
/// cannot decode this fast" — which need opposite responses.
///
/// Sent on a timer, not per frame: it is a control signal, and adding a message
/// per frame would cost bandwidth in exactly the situation it exists to detect.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct GuestTelemetry {
    /// Median milliseconds to decode a frame.
    pub decode_ms: f32,
    /// Median milliseconds to upload and draw a decoded frame.
    pub render_ms: f32,
    /// Frames actually presented per second over the reporting window.
    pub displayed_fps: f32,
    /// Round trip to the host in milliseconds, from the ping/pong pair.
    pub rtt_ms: f32,
}

/// Host → guest: what the stream is running at, and what is holding it back.
///
/// The guest can measure its own decode and the round trip, but it cannot see
/// whether the host is capture-bound or encode-bound — and those look identical
/// from here (a stuttering picture and an idle bandwidth graph). Sending the
/// host's own diagnosis is what lets the guest's toolbar name the real cause
/// rather than blaming the network by default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamStatus {
    pub session_id: u32,
    /// Packed [`crate::adaptive::BottleneckSet`]. Unknown bits are ignored, so
    /// a newer host can add stages without breaking an older guest.
    pub bottlenecks: u8,
    /// Frames per second the host is currently targeting.
    pub fps: u32,
    /// Bitrate ceiling currently in force, in kbps.
    pub bitrate_kbps: u32,
    /// True when the stream is running below what the host was asked for,
    /// because something could not keep up.
    pub degraded: bool,
}

/// Host → guest: the stream has changed codec mid-session.
///
/// # Why this exists rather than reconnecting
///
/// The guest builds its decoder from the codec named in [`SessionStart`] and
/// cannot switch under it — a bitstream it has no decoder for produces nothing.
/// Without this, comparing two codecs meant disconnecting and reconnecting for
/// each one, which is a poor way to compare anything.
///
/// Sent *before* the first frame of the new codec, so the guest is never handed
/// something it cannot read. The guest rebuilds and asks for a keyframe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecChanged {
    /// The codec the host is switching to, as [`crate::VideoCodec::wire_name`].
    pub encoder_type: String,
}

/// Full-screen information broadcast at session start (host → guest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub width: u16,
    pub height: u16,
    /// DPI scale factor (e.g. 1.5 for a HiDPI display).
    pub dpi_scale: f32,
}

/// Host-to-guest: session has started and screen info is available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStart {
    pub session_id: u32,
    pub screen_info: ScreenInfo,
    /// Encoder type the host will use for frames ("h264" or "raw").
    pub encoder_type: String,
    /// What the host calls itself, if it publishes a name. The guest saves this
    /// against the connection so its list reads as machines rather than hex.
    pub host_name: Option<String>,
}

// ── Reachability probe ───────────────────────────────────────

/// Guest → host: is this code still good, and are you still there?
///
/// Deliberately **not** an authentication attempt. A saved connection needs a
/// live answer to two separate questions — can this host be reached, and is the
/// code still the current one — and the only way to find out used to be to dial
/// it for real, which raises an approval prompt on someone else's screen. A card
/// cannot honestly show a state it would have to interrupt a person to learn.
///
/// The host answers and closes: no session is created, nothing is streamed, and
/// the host's user is never told it happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub passcode: String,
}

/// Host → guest: the answer to a [`Probe`].
///
/// Reachability is implied by getting a reply at all, so it is not a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Whether the code is one this host currently accepts.
    pub valid: bool,
    /// What the host calls itself. Only sent when the code is valid — a wrong
    /// guess learns nothing about the machine it was aimed at.
    pub host_name: Option<String>,
    /// Whether connecting would wait for the host to allow it, rather than
    /// starting immediately. Lets the card say "will ask for approval".
    pub needs_approval: bool,
}

/// Connection closed gracefully.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEnd {
    pub session_id: u32,
    pub reason: String,
}

// ── Top-level message enum ───────────────────────────────────

/// All messages exchanged over the iroh QUIC stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// A small blurred picture of the host's screen, sent once per session.
    ///
    /// Exactly [`super::preview::BYTES`] of RGB at
    /// [`super::preview::W`] x [`super::preview::H`]. The guest keeps it against
    /// this host's saved connection so the card is recognisable later.
    ///
    /// Sent by the host rather than sampled by the guest for two reasons: the
    /// guest may never see a frame on the processor at all — a decoder drawing
    /// straight from the GPU never reads one back — and blurring at the source
    /// means the sharp original never goes on the wire.
    HostPreview(Vec<u8>),

    // --- Guest → Host ---
    /// Initial passcode authentication (sent as first message on connection).
    Auth {
        passcode: String,
        session_id: u32,
        /// What the guest calls itself. `None` means "stay anonymous", and the
        /// host falls back to a generated label.
        client_name: Option<String>,
    },
    /// Display configuration from guest.
    Config(DisplayConfig),
    /// Input events from guest.
    Input(InputEvent),
    /// Periodic report on how the guest's own side of the pipeline is doing.
    Telemetry(GuestTelemetry),
    /// This frame has been decoded and handed to the display.
    ///
    /// The host uses these to bound how far ahead of the guest it may get. See
    /// the flow-control note in `network::spawn_capture_pipeline`.
    FrameAck(u32),
    /// This guest cannot decode any more and needs a frame that stands alone.
    ///
    /// # Why the guest asks rather than the host guessing
    ///
    /// The stream is carried on a reliable, ordered QUIC stream, so nothing is
    /// ever lost between the encoder and the decoder. The only way a guest loses
    /// sync is its own decoder failing on a frame that arrived intact — which
    /// the host cannot see and cannot predict.
    ///
    /// Sending keyframes on a timer against that possibility cost a burst
    /// several times the size of a normal frame, on a fixed period, on every
    /// session — a visible hitch bought as insurance against something that
    /// announces itself perfectly well.
    RequestKeyframe,
    /// Check a code without asking to connect. Answered with [`ProbeResult`],
    /// then the connection closes.
    Probe(Probe),

    // --- Host → Guest ---
    SessionStart(SessionStart),
    /// The stream changed codec; rebuild the decoder. See [`CodecChanged`].
    CodecChanged(CodecChanged),
    Frame(FrameMessage),
    GuestApprovalRequest(GuestApprovalRequest),
    /// Input permissions for this session were granted or revoked.
    ControlState(ControlStateMessage),
    /// What the stream is running at and what is limiting it.
    StreamStatus(StreamStatus),
    /// One or more Opus packets of the host's system audio.
    Audio(AudioMessage),
    /// Where the host's pointer is, and what it looks like.
    ///
    /// Sent separately from the frame rather than composited into it, so the
    /// guest can draw it sharp and at its own rate. See [`super::cursor`].
    Cursor(super::cursor::CursorUpdate),
    /// Answer to a [`Probe`].
    ProbeResult(ProbeResult),
    SessionEnd(SessionEnd),

    // --- Bidirectional heartbeat ---
    Ping(u64),
    /// Answer to a [`Message::Ping`], carrying both clocks.
    ///
    /// # Why the host's clock is in here
    ///
    /// A frame is stamped with the host's wall clock, and the guest has its
    /// own — two machines' clocks differ by anything from milliseconds to
    /// minutes, so subtracting one from the other measures the disagreement,
    /// not the latency. Sending the host's clock alongside the echo lets the
    /// guest work out the offset the same way NTP does, and only then is
    /// "capture to display" a real number rather than a plausible one.
    Pong {
        /// The guest clock value from the ping, echoed back.
        echo: u64,
        /// The host's clock when it answered, in the same units.
        host_ms: u64,
    },
}

/// A block of encoded audio from the host.
///
/// # Why the position and not a timestamp
///
/// `first_frame` counts audio frames since the session started, including
/// silence, taken from the capture side's own running total. A wall-clock
/// timestamp would carry the sender's scheduling jitter into the receiver's
/// clock; a frame count is exactly the presentation position and nothing else,
/// which is what a jitter buffer needs to detect a gap and what A/V sync needs
/// to place the picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioMessage {
    /// Opus packets, each behind a two-byte big-endian length.
    pub packets: Vec<u8>,
    /// Frame position of the first sample in this block.
    pub first_frame: u64,
    /// Host wall clock when this block was captured.
    ///
    /// The same clock as [`FrameMessage::timestamp_ms`], which is the whole
    /// point: without a shared reference there is no way to say whether sound
    /// is ahead of or behind picture.
    pub captured_ms: u64,
    /// Frames this block covers, so a receiver can tell that packets went
    /// missing without decoding them.
    pub frames: u32,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Maximum message size in bytes.
///
/// Sized for a worst-case keyframe: uncompressed 4K BGRA is 33 MiB, and a
/// compressed keyframe of a photographic desktop can be a large fraction of
/// that. The old 10 MiB cap was fine for a 640×360 test pattern but would kill
/// a session the moment real capture ran at native resolution.
///
/// It still exists to bound what a peer can make us allocate from a length
/// prefix alone, before anything is authenticated.
pub const MAX_MESSAGE_SIZE: usize = 48 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_map_to_the_device_they_need_permission_for() {
        assert_eq!(InputEvent::MouseMove(MouseEvent { x: 0, y: 0 }).device(), InputDevice::Mouse);
        assert_eq!(
            InputEvent::MouseButton(MouseButtonEvent { button: 0, pressed: true }).device(),
            InputDevice::Mouse
        );
        assert_eq!(InputEvent::Scroll(ScrollEvent { dx: 0, dy: 1 }).device(), InputDevice::Mouse);
        assert_eq!(
            InputEvent::Key(KeyEvent {
                keycode: 65,
                pressed: true,
                modifiers: 0,
                scancode: 0,
                extended: false,
            })
            .device(),
            InputDevice::Keyboard
        );
    }

    #[test]
    fn grants_gate_each_device_independently() {
        let mouse_only = ControlGrants::new(true, false);
        assert!(mouse_only.allows(InputDevice::Mouse));
        assert!(!mouse_only.allows(InputDevice::Keyboard));

        let kbd_only = ControlGrants::new(false, true);
        assert!(!kbd_only.allows(InputDevice::Mouse));
        assert!(kbd_only.allows(InputDevice::Keyboard));

        assert!(!ControlGrants::NONE.any());
        assert!(ControlGrants::ALL.allows(InputDevice::Mouse));
        assert!(ControlGrants::ALL.allows(InputDevice::Keyboard));
        assert_eq!(ControlGrants::default(), ControlGrants::NONE);
    }

    #[test]
    fn the_message_cap_admits_a_worst_case_keyframe() {
        // A frame larger than the cap tears down the session, so the cap has to
        // clear uncompressed 4K — the largest thing the pipeline can produce.
        let uncompressed_4k = 3840usize * 2160 * 4;
        assert!(
            MAX_MESSAGE_SIZE > uncompressed_4k,
            "cap of {MAX_MESSAGE_SIZE} cannot carry a {uncompressed_4k}-byte 4K frame"
        );
    }

    #[test]
    fn a_peer_name_is_bounded_and_stripped_of_control_characters() {
        assert_eq!(sanitize_device_name("Studio PC"), Some("Studio PC".into()));
        assert_eq!(sanitize_device_name("  padded  "), Some("padded".into()));
        // A newline in a name would break out of the label it is drawn in.
        assert_eq!(sanitize_device_name("evil\nname"), Some("evilname".into()));
        assert_eq!(sanitize_device_name("   "), None);
        assert_eq!(sanitize_device_name(""), None);

        let long = "x".repeat(500);
        assert_eq!(sanitize_device_name(&long).unwrap().len(), MAX_DEVICE_NAME);
    }

    #[test]
    fn control_state_roundtrips_through_bincode() {
        let msg = Message::ControlState(ControlStateMessage {
            session_id: 5,
            grants: ControlGrants::new(true, false),
        });
        let bytes = bincode::serialize(&msg).unwrap();
        match bincode::deserialize::<Message>(&bytes).unwrap() {
            Message::ControlState(cs) => {
                assert_eq!(cs.session_id, 5);
                assert_eq!(cs.grants, ControlGrants::new(true, false));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
