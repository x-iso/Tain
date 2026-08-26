//! Guest-side session: connect to a host, receive frames, forward input.
//!
//! Runs entirely on the tokio runtime and communicates with the egui thread
//! through the `Arc<Mutex<_>>` handles in [`GuestSessionState`]. The render pass
//! never blocks on the network, and the network never touches egui state beyond
//! requesting a repaint.

use anyhow::{Context as _, Result};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::*;

use tain_core::codec_delta::DeltaDecoder;
use tain_core::codec_h264::H264Decoder;
use tain_core::network::{read_stream_message, write_stream_message};
use tain_core::protocol::{DisplayConfig, GuestTelemetry, InputEvent, Message, StreamStatus};
use tain_core::{BottleneckSet, ConnectionCode, ControlGrants, GuestClient, VideoCodec};

/// Where a guest session currently is. Drives what the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestStatus {
    Disconnected,
    Connecting,
    /// Authenticated; waiting for the host's first frame.
    Authenticated,
    /// The host has been asked and has not answered. Nothing is streaming yet —
    /// this is a distinct state from "connected but no video", because the
    /// waiting is someone else's decision rather than a stall.
    AwaitingApproval,
    /// Frames are arriving.
    Streaming,
    Failed(String),
}

impl GuestStatus {
    /// Short text for the header's status line.
    pub fn label(&self) -> String {
        match self {
            Self::Disconnected => "Disconnected".into(),
            Self::Connecting => "Connecting…".into(),
            Self::Authenticated => "Authenticated — waiting for video".into(),
            Self::AwaitingApproval => "Waiting for the host to allow this connection…".into(),
            Self::Streaming => "Connected".into(),
            Self::Failed(e) => format!("Failed: {e}"),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Authenticated | Self::AwaitingApproval | Self::Streaming
        )
    }
}

/// The host's pointer, as last reported.
///
/// # Why the pointer is not in the picture
///
/// The host sends position and shape rather than compositing the cursor into
/// the frame. Drawing it here means it moves at *this* machine's frame rate
/// instead of the stream's, stays sharp instead of being blurred by lossy
/// compression, and does not dirty an otherwise static desktop every time the
/// mouse twitches.
#[derive(Default)]
pub struct CursorState {
    pub position: Option<tain_core::cursor::CursorPosition>,
    /// The current image. `None` until the host has sent one.
    pub shape: Option<tain_core::cursor::CursorShape>,
    /// Uploaded texture, rebuilt when the shape id changes.
    pub texture: Option<(u64, egui::TextureHandle)>,
}

/// One decoded frame, ready to be drawn.
///
/// Two shapes, because there are two ways a frame gets to the screen. The
/// pixels are read back to the CPU on any machine that cannot share Direct3D
/// textures with OpenGL; where it can, the frame stays on the GPU and only a
/// handle travels.
pub struct FrameBuffer {
    pub width: usize,
    pub height: usize,
    /// RGBA8, `width * height * 4` bytes. Empty when `gpu` is set.
    pub rgba: Vec<u8>,
    /// The `ID3D11Texture2D` holding this frame and the converter generation it
    /// belongs to, when it was never read back.
    ///
    /// A `usize` rather than a pointer so this stays `Send`: it crosses from the
    /// decode task to the render thread, and nothing on the way dereferences it.
    /// The generation travels with it because the pointer alone does not
    /// identify a texture across a rebuild — see
    /// `tain_core::decode_ffmpeg::GpuPicture`.
    pub gpu: Option<(usize, u64)>,
}

/// How the stream is doing, from both ends.
///
/// The measurements are taken here — decode cost on the receive task, draw cost
/// and presented frame rate on the render pass — and reported to the host, which
/// combines them with its own and sends back the diagnosis. The host is the only
/// side that can see all the stages, so it is the single authority on *which*
/// one is the problem; this side just measures and displays.
#[derive(Default)]
pub struct StreamHealth {
    /// The host's diagnosis, once it has sent one.
    pub status: Option<StreamStatus>,
    /// Round trip in milliseconds, from the last ping/pong pair.
    pub rtt_ms: f32,
    /// What the stream is actually doing, refreshed once a second.
    ///
    /// Separate from the report sent to the host because the UI reads it at any
    /// time and the report drains its accumulators.
    pub live: LiveStats,
    /// When anything last arrived from the host, for the stall watchdog.
    pub last_message: Option<std::time::Instant>,
    /// How the connection is carried, and the advice if it is a relay.
    pub path: Option<tain_core::network::ConnectionPath>,
    /// How far the host's clock is ahead of ours, in milliseconds.
    ///
    /// `None` until a pong has been seen. Estimated the way NTP does it, from
    /// the round trip: with the request and reply legs assumed symmetric, the
    /// host's clock at the midpoint of the exchange is the best available
    /// reading of it.
    clock_offset_ms: Option<f64>,
    /// End-to-end capture-to-arrival samples for the current window.
    latency_ms: Vec<f32>,
    /// Host capture time of the most recent video frame and audio block, so the
    /// two can be compared.
    last_video_ms: Option<u64>,
    last_audio_ms: Option<u64>,
    /// Milliseconds sound trails picture. Positive means audio is behind.
    av_offset_ms: Vec<f32>,
    decode_ms: Vec<f32>,
    render_ms: Vec<f32>,
    displayed_frames: u32,
    /// Frames decoded here and then overwritten before the window drew them.
    ///
    /// The one loss in the pipeline nothing else can see. The host counts what
    /// it sent, the guest counts what it decoded, and both are happy — while the
    /// picture visibly hitches, because a frame was produced, carried, decoded
    /// and then thrown away at the last step.
    skipped_frames: u32,
    received_bytes: usize,
    window_start: Option<std::time::Instant>,
}

/// The numbers worth putting on screen.
///
/// Every one of these was needed to work out why a real session was unusable,
/// and none of them were visible in the app at the time. A remote desktop that
/// feels slow and cannot say why leaves the user guessing between their network,
/// their hardware and a bug.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveStats {
    /// Frames actually drawn per second.
    pub displayed_fps: f32,
    /// Frames a second that were decoded and then never drawn.
    ///
    /// Should be zero. Anything else is the presentation stage discarding work
    /// the whole rest of the pipeline paid for — see [`StreamHealth::note_skipped`].
    pub skipped_fps: f32,
    /// Median milliseconds to decode a frame.
    pub decode_ms: f32,
    /// Median milliseconds to upload and draw one.
    pub render_ms: f32,
    /// Video actually arriving, in kbit/s.
    pub received_kbps: f32,
    /// Median milliseconds from the host capturing a frame to it arriving here.
    ///
    /// The number a user actually feels, and the only one that includes the
    /// host's own pipeline. Zero until the clock offset has been established.
    pub latency_ms: f32,
    /// Milliseconds sound trails picture. Positive means audio is behind.
    pub av_offset_ms: f32,
}

/// Median of a sample window, or zero when there is nothing in it.
fn median_of(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[sorted.len() / 2]
}

/// What became of every frame that arrived, since the last report.
///
/// A stream that has stopped moving looks identical from the outside whatever
/// killed it: no frames arriving, frames arriving that produce no picture, or
/// pictures that never reach the screen. Only the guest can tell those apart,
/// and until this existed it said nothing at all — `Ok(None)` from a decoder is
/// a routine, silent outcome, so a decoder that stopped producing pictures
/// froze the picture without a single line in the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameTally {
    /// Frames taken off the wire.
    pub arrived: u32,
    /// Frames that produced a picture.
    pub decoded: u32,
    /// Frames the decoder accepted but produced no picture for — buffering, or
    /// a parameter set. Routine in ones and twos; damning if it is all of them.
    pub no_picture: u32,
    /// Frames the decoder rejected.
    pub failed: u32,
    /// Keyframes among the arrivals. The host sends one to recover, so a stream
    /// stuck behind a decode failure shows these arriving and still not moving.
    pub keyframes: u32,
    pub bytes: usize,
}

impl FrameTally {
    /// One line saying where the frames went, or `None` if none arrived.
    fn summary(&self, seconds: f32) -> Option<String> {
        if self.arrived == 0 {
            return None;
        }
        let per_sec = |n: u32| n as f32 / seconds.max(0.001);
        let mut line = format!(
            "video: {:.1} arrived/s, {:.1} decoded/s, {:.0} kbit/s",
            per_sec(self.arrived),
            per_sec(self.decoded),
            self.bytes as f32 * 8.0 / seconds.max(0.001) / 1000.0,
        );
        // Only mentioned when they happen, so a healthy line stays readable and
        // an unhealthy one is obvious at a glance.
        if self.no_picture > 0 {
            line.push_str(&format!(", {} produced no picture", self.no_picture));
        }
        if self.failed > 0 {
            line.push_str(&format!(", {} failed to decode", self.failed));
        }
        if self.keyframes > 0 {
            line.push_str(&format!(", {} keyframes", self.keyframes));
        }
        Some(line)
    }
}

impl StreamHealth {
    /// Fold one ping/pong exchange into the clock estimate.
    ///
    /// Smoothed rather than replaced: a single exchange that happened to be
    /// delayed on one leg reads as a clock jump, and a latency figure that
    /// leaps about is worse than none.
    pub fn note_clock(&mut self, sent: u64, host_ms: u64, arrived: u64) {
        // Midpoint of our own send and receive, which is when the host answered
        // if the two legs took the same time.
        let midpoint = (sent as f64 + arrived as f64) / 2.0;
        let offset = host_ms as f64 - midpoint;
        self.clock_offset_ms = Some(match self.clock_offset_ms {
            Some(previous) => previous * 0.8 + offset * 0.2,
            None => offset,
        });
    }

    /// Record how old a frame was when it arrived.
    ///
    /// Silently ignored until the clock offset is known — a number derived from
    /// two unsynchronised clocks is not a measurement.
    pub fn note_frame_latency(&mut self, captured_host_ms: u64, arrived_ms: u64) {
        let Some(offset) = self.clock_offset_ms else { return };
        self.last_video_ms = Some(captured_host_ms);
        let captured_here = captured_host_ms as f64 - offset;
        let latency = arrived_ms as f64 - captured_here;
        // A negative or absurd figure means the estimate has not settled, or
        // a clock stepped underneath us. Better dropped than displayed.
        if (0.0..2000.0).contains(&latency) {
            self.latency_ms.push(latency as f32);
        }
    }

    /// Note when an audio block was captured, and how much is queued behind it.
    ///
    /// # Why video is not delayed to match
    ///
    /// A media player syncs picture to sound because a fixed offset is
    /// invisible and lip sync is not. A remote desktop is the opposite: the
    /// user is *driving* it, and holding video back to match the audio buffer
    /// would add that buffer to the delay between moving the mouse and seeing
    /// it move — the one thing that must stay small.
    ///
    /// So the offset is measured and corrected from the audio side, by keeping
    /// its buffer short (see `audio_render`), and video is always presented as
    /// soon as it arrives.
    pub fn note_audio(&mut self, captured_ms: u64, buffered_ms: u32) {
        self.last_audio_ms = Some(captured_ms);
        let Some(video) = self.last_video_ms else { return };
        // Audio still queued has not been heard yet, so it is that much further
        // behind than its capture time alone suggests.
        let offset = video as f64 - captured_ms as f64 + buffered_ms as f64;
        if offset.abs() < 2000.0 {
            self.av_offset_ms.push(offset as f32);
        }
    }

    pub fn record_decode(&mut self, ms: f32, bytes: usize) {
        self.decode_ms.push(ms);
        self.received_bytes += bytes;
    }

    /// A decoded frame was replaced before anything drew it.
    pub fn note_skipped(&mut self) {
        self.skipped_frames += 1;
    }

    /// Called from the render pass when a frame is actually uploaded and drawn.
    pub fn record_render(&mut self, ms: f32) {
        self.render_ms.push(ms);
        self.displayed_frames += 1;
    }

    /// Median decode cost over the current window, for logging.
    ///
    /// Reads the same samples the telemetry report drains, so the number in the
    /// log is the number the host adapted on rather than a second measurement
    /// that could disagree with it.
    fn last_decode_ms(&self) -> f32 {
        median_of(&self.decode_ms)
    }

    /// Median draw cost over the current window, for logging.
    fn last_render_ms(&self) -> f32 {
        median_of(&self.render_ms)
    }

    /// Stages the host says are struggling. Empty until it has told us.
    pub fn bottlenecks(&self) -> BottleneckSet {
        self.status
            .map(|s| BottleneckSet::from_bits(s.bottlenecks))
            .unwrap_or(BottleneckSet::NONE)
    }

    /// True when the host is streaming below what it was asked for.
    pub fn is_degraded(&self) -> bool {
        self.status.is_some_and(|s| s.degraded)
    }

    /// Drain the window into a report for the host.
    fn take_report(&mut self) -> GuestTelemetry {
        let median = |values: &mut Vec<f32>| {
            if values.is_empty() {
                return 0.0;
            }
            values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = values[values.len() / 2];
            values.clear();
            mid
        };

        let now = std::time::Instant::now();
        let elapsed = self
            .window_start
            .map(|start| now.duration_since(start).as_secs_f32())
            .unwrap_or(0.0);
        self.window_start = Some(now);

        let report = GuestTelemetry {
            decode_ms: median(&mut self.decode_ms),
            render_ms: median(&mut self.render_ms),
            // A window with no elapsed time yet reports nothing rather than
            // dividing by zero into an absurd frame rate the host would act on.
            displayed_fps: if elapsed > 0.05 {
                self.displayed_frames as f32 / elapsed
            } else {
                0.0
            },
            // Everything that arrived and decoded, whether or not a repaint
            // came along in time to draw it. See `GuestTelemetry::decoded_fps`.
            decoded_fps: if elapsed > 0.05 {
                (self.displayed_frames + self.skipped_frames) as f32 / elapsed
            } else {
                0.0
            },
            rtt_ms: self.rtt_ms,
        };

        self.live = LiveStats {
            displayed_fps: report.displayed_fps,
            skipped_fps: if elapsed > 0.05 {
                self.skipped_frames as f32 / elapsed
            } else {
                0.0
            },
            decode_ms: report.decode_ms,
            render_ms: report.render_ms,
            received_kbps: if elapsed > 0.05 {
                self.received_bytes as f32 * 8.0 / 1000.0 / elapsed
            } else {
                0.0
            },
            latency_ms: median(&mut self.latency_ms),
            av_offset_ms: median(&mut self.av_offset_ms),
        };
        self.displayed_frames = 0;
        self.skipped_frames = 0;
        self.received_bytes = 0;
        report
    }
}

/// State shared between the session task and the egui render pass.
#[derive(Clone)]
pub struct GuestSessionState {
    pub status: Arc<Mutex<GuestStatus>>,
    /// Latest frame. The renderer `take()`s it, so a texture upload only happens
    /// when a new frame actually arrived rather than on every repaint.
    pub frame: Arc<Mutex<Option<FrameBuffer>>>,
    /// Input permissions the host has granted us, per `Message::ControlState`.
    pub grants: Arc<Mutex<ControlGrants>>,
    /// Host screen dimensions, for scaling pointer coordinates.
    pub remote_size: Arc<Mutex<(u16, u16)>>,
    /// Where the host's pointer is and what it looks like.
    pub cursor: Arc<Mutex<CursorState>>,
    /// A blurred thumbnail of the host's screen, sent once when the session
    /// starts. Kept against this host's saved connection when it closes.
    pub host_preview: Arc<Mutex<Option<Vec<u8>>>>,
    /// Playback for this session, opened when the first audio block arrives.
    #[cfg(all(target_os = "windows", db_ffmpeg))]
    pub audio: Arc<Mutex<Option<tain_core::audio_output::AudioOutput>>>,
    /// Set once opening a playback device has failed, so it is tried once.
    pub audio_failed: Arc<std::sync::atomic::AtomicBool>,
    /// The decoder's D3D11 device, once a GPU decoder has been built.
    ///
    /// Zero until then. Published so the render thread can open it for sharing
    /// with OpenGL — that has to happen on the thread holding the GL context,
    /// and the decoder lives on this one.
    pub gpu_device: Arc<std::sync::atomic::AtomicUsize>,
    /// Set by the render thread once it can draw the decoder's textures itself.
    ///
    /// The decode loop reads it each frame and stops reading frames back. A
    /// running flag rather than a construction argument because the answer is
    /// only known after the render thread has seen a device, which is after the
    /// first frames have already been decoded.
    pub present_on_gpu: Arc<std::sync::atomic::AtomicBool>,
    /// This machine's quality-versus-latency position, 0-100, for the jitter
    /// buffer to size itself from.
    ///
    /// The guest's own setting rather than the host's: the buffer trades *this*
    /// machine's audio delay against *this* machine's dropouts, and the host has
    /// no stake in that. Read when playback opens, which is when the first block
    /// arrives.
    pub audio_priority: Arc<std::sync::atomic::AtomicU8>,
    /// How large the video is actually drawn, in physical pixels.
    ///
    /// Written by the render pass, read by the writer task, which tells the
    /// host. Anything beyond this is detail that is scaled away before anyone
    /// sees it — and paid for four times on the way: bitrate, decode, colour
    /// conversion and texture upload.
    pub viewport: Arc<Mutex<Option<(u16, u16)>>>,
    /// What the host called itself in `SessionStart`, once it has said.
    pub host_name: Arc<Mutex<Option<String>>>,
    /// Measurements from this side, and the host's diagnosis coming back.
    pub health: Arc<Mutex<StreamHealth>>,
    /// The codec the host said it is sending, once it has said.
    ///
    /// # Why this is shared rather than local to a reader
    ///
    /// The host names its codec in `SessionStart`, which travels on the control
    /// stream. Frames travel on their own stream now, read by a second loop
    /// with its own decoder — and that loop never sees `SessionStart`, so it
    /// never learned what it was decoding. Every frame was refused with "the
    /// host sent a frame before saying which codec it uses": eight frames a
    /// second arriving, none decoded, a session stuck on "waiting for video".
    ///
    /// Splitting the streams was right; leaving the codec in one reader's local
    /// state was what broke. It belongs to the session, not to whichever loop
    /// happened to hear about it, and `CodecChanged` mid-session reaches the
    /// frame reader the same way.
    pub codec: Arc<Mutex<Option<VideoCodec>>>,
}

impl Default for GuestSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestSessionState {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(GuestStatus::Disconnected)),
            frame: Arc::new(Mutex::new(None)),
            grants: Arc::new(Mutex::new(ControlGrants::NONE)),
            remote_size: Arc::new(Mutex::new((640, 360))),
            cursor: Arc::new(Mutex::new(CursorState::default())),
            host_preview: Arc::new(Mutex::new(None)),
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            audio: Arc::new(Mutex::new(None)),
            audio_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            gpu_device: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            present_on_gpu: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            audio_priority: Arc::new(std::sync::atomic::AtomicU8::new(
                tain_core::LatencyPriority::BALANCED.value(),
            )),
            viewport: Arc::new(Mutex::new(None)),
            host_name: Arc::new(Mutex::new(None)),
            codec: Arc::new(Mutex::new(None)),
            health: Arc::new(Mutex::new(StreamHealth::default())),
        }
    }

    pub fn status(&self) -> GuestStatus {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, status: GuestStatus) {
        *self.status.lock().unwrap() = status;
    }

    pub fn reset(&self) {
        self.set_status(GuestStatus::Disconnected);
        *self.frame.lock().unwrap() = None;
        *self.grants.lock().unwrap() = ControlGrants::NONE;
        *self.host_name.lock().unwrap() = None;
        *self.health.lock().unwrap() = StreamHealth::default();
    }
}

/// How often the guest reports its own measurements and pings the host.
///
/// Slightly slower than the host's evaluation window, so a report is always
/// waiting when the controller looks — and slow enough that the reporting itself
/// is invisible next to the video.
const TELEMETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// How long the host may say nothing before the guest gives up on it.
///
/// The host sends frames, control state and pong replies, so silence this long
/// means the connection is dead however healthy QUIC still believes it is.
/// Generous enough to survive a stalled relay that recovers.
const HOST_SILENCE_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long hole punching gets before a relayed path is worth complaining about.
///
/// Every iroh connection starts relayed and upgrades if it can, so a verdict
/// reached at connect time is not a verdict, it is a stopwatch that has not
/// started. Long enough for a direct path across an ocean, short enough that
/// someone stuck on a relay is told before they have given up on the session.
const DIRECT_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

/// Milliseconds since the epoch, for round-trip timing.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Connect to the host a code addresses and run the session to completion.
///
/// Returns when the host closes the stream or an error occurs. Cancel by
/// aborting the task — dropping the connection is a clean disconnect.
pub async fn run_guest_session(
    code: ConnectionCode,
    known_addrs: Vec<std::net::SocketAddr>,
    client_name: Option<String>,
    shared: GuestSessionState,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    repaint: impl Fn() + Send + Sync + 'static,
) -> Result<()> {
    shared.set_status(GuestStatus::Connecting);

    let result = async {
        let mut client = GuestClient::connect_with_code(&code, &known_addrs)
            .await
            .context("Could not reach that host")?;

        // The host authenticates on the secret inside the code, so send it whole.
        let (mut send, mut recv) =
            client.authenticate(&code.encode(), client_name.as_deref()).await?;
        shared.set_status(GuestStatus::Authenticated);

        // Recorded, but deliberately not judged yet.
        //
        // # Why there is no relay warning here
        //
        // There used to be, and it fired on every single session. This runs
        // about 200 ms after the connection opens, and iroh has not finished
        // hole punching by then — *every* connection starts on the relay and
        // upgrades if it can. So the log said "could not establish a direct
        // connection" for connections that went direct a second later, and the
        // advice it gave (blame a VPN) sent people looking for a problem they
        // did not have.
        //
        // The verdict is left to the heartbeat below, which asks again once
        // there has been time for an answer.
        if let Some(path) = client.connection_path() {
            shared.health.lock().unwrap().path = Some(path);
        }

        // Tell the host what we can display, if the window has been drawn yet.
        //
        // Deliberately *not* `remote_size`, which at this point is a placeholder
        // for a host we have not heard from — sending that told the host we
        // could display 640x360 and it ignored the whole message anyway. Zeroes
        // mean "not known yet"; the writer sends the real size as soon as the
        // first frame has been laid out.
        let mut reported_viewport = *shared.viewport.lock().unwrap();
        write_stream_message(&mut send, &Message::Config(viewport_config(reported_viewport)))
            .await?;

        // # Audio has its own reader on its own stream
        //
        // QUIC orders within a stream but not across them, so a keyframe on the
        // video stream cannot delay an Opus packet on this one. Read by its own
        // task for the same reason — sharing the video reader would put it back
        // behind whatever that was in the middle of.
        //
        // ## Why this does not wait with a deadline
        //
        // It used to accept the stream inline with a two second timeout, which
        // raced the host's approval prompt: the host only opens the stream once
        // the session is approved, so any connection a human had to click
        // through lost its audio permanently and silently. A master passcode
        // skips approval, so audio worked there and nowhere else — which is
        // exactly the difference the first two-machine test turned up.
        //
        // A host that never opens one is an older build or one with no audio
        // device; this task simply waits forever and is aborted with the session.
        // Reader owns `recv` exclusively — read_stream_message is not cancel-safe.
        // Acks travel back through the writer, which owns the send half.
        //
        // Declared before the side streams because video arrives on one of them
        // now and acking is what keeps the host from running ahead of this
        // decoder.
        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<u32>();

        // Shared rather than moved: the picture can arrive on the main stream
        // or on the video stream, and both have to be able to ask for a repaint.
        let repaint = std::sync::Arc::new(repaint);

        // # One acceptor for every side stream the host opens
        //
        // There are two now, sound and pictures, and which arrives first is not
        // something to depend on: a host that opened them in the other order,
        // or failed to open one, would have video decoded as audio. Not an
        // error — noise and a black screen. So each stream says what it carries
        // in its first byte and this dispatches on that.
        //
        // A host that opens neither is one with no sound device on a link that
        // refused a second stream; this task simply waits, and everything falls
        // back to the main stream as it did before there were any.
        let side_streams = client.connection().map(|conn| {
            let side_state = shared.clone();
            let side_acks = ack_tx.clone();
            let side_repaint = repaint.clone();
            tokio::spawn(async move {
                loop {
                    let Ok(mut stream) = conn.accept_uni().await else {
                        debug!("No more side streams from the host");
                        return;
                    };

                    let mut tag = [0u8; 1];
                    if tokio::io::AsyncReadExt::read_exact(&mut stream, &mut tag).await.is_err() {
                        debug!("A side stream closed before saying what it carried");
                        continue;
                    }

                    let state = side_state.clone();
                    let acks = side_acks.clone();
                    let repaint = side_repaint.clone();
                    match tag[0] {
                        tain_core::protocol::stream_kind::AUDIO => {
                            tokio::spawn(async move {
                                // Nothing here acks or repaints: audio drives
                                // the speakers, not the screen, and a picture is
                                // not owed for a sound.
                                let (tx, _rx) = mpsc::unbounded_channel::<u32>();
                                let (control_tx, _rx) = mpsc::unbounded_channel::<Message>();
                                if let Err(e) =
                                    receive_loop(&mut stream, &state, tx, control_tx, || {}).await
                                {
                                    debug!("Guest audio stream ended: {e}");
                                }
                            });
                        }
                        tain_core::protocol::stream_kind::VIDEO => {
                            tokio::spawn(async move {
                                // A keyframe request cannot come from here —
                                // the writer owns the control stream — so the
                                // sender goes nowhere, as it does for audio.
                                let (control_tx, _rx) = mpsc::unbounded_channel::<Message>();
                                if let Err(e) =
                                    receive_loop(&mut stream, &state, acks, control_tx, move || {
                                        repaint()
                                    })
                                    .await
                                {
                                    debug!("Guest video stream ended: {e}");
                                }
                            });
                        }
                        other => debug!("Ignoring a side stream of unknown kind {other}"),
                    }
                }
            })
        });

        // Control messages the receive side needs to send back — currently only
        // a keyframe request, which cannot go down the ack channel because that
        // one carries a sequence number rather than a message.
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Message>();
        let reader_state = shared.clone();
        let reader = tokio::spawn(async move {
            if let Err(e) =
                receive_loop(&mut recv, &reader_state, ack_tx, control_tx, move || repaint()).await
            {
                debug!("Guest receive loop ended: {e}");
            }
        });

        // Forward input, and report on our own health, until the channel closes
        // or the reader stops.
        let writer_state = shared.clone();
        let started = std::time::Instant::now();
        // One line per session describing how it is actually reaching the other
        // machine, said once hole punching has had time to settle.
        let mut reported_path = false;
        let outcome = tokio::select! {
            _ = async {
                let mut heartbeat = tokio::time::interval(TELEMETRY_INTERVAL);
                // The first tick fires immediately; skip it so the opening
                // report is not an empty window.
                heartbeat.tick().await;
                loop {
                    let message = tokio::select! {
                        // Acks first: they gate how far ahead the host may get,
                        // so delaying them directly costs frame rate.
                        biased;
                        ack = ack_rx.recv() => match ack {
                            Some(sequence) => Message::FrameAck(sequence),
                            None => break,
                        },
                        // Second: a guest that cannot decode is not streaming at
                        // all until this arrives, so it outranks input.
                        control = control_rx.recv() => match control {
                            Some(message) => message,
                            None => break,
                        },
                        event = input_rx.recv() => match event {
                            Some(event) => Message::Input(event),
                            None => break,
                        },
                        _ = heartbeat.tick() => {
                            // Nothing at all from the host for this long means
                            // it is gone. A guest working through a backlog used
                            // to keep drawing — and keep claiming to be
                            // connected — long after the host had quit.
                            //
                            // # Except while an approval is outstanding
                            //
                            // The host says nothing at all between asking its
                            // user and getting an answer — it is not even
                            // reading the stream, so pings are not replied to
                            // either. Silence there is the normal case, and
                            // treating it as a dead host meant every guest gave
                            // up ten seconds in, which is less time than it
                            // takes to notice a dialog. Reported as being
                            // disconnected the moment approval was granted.
                            //
                            // Nothing is lost by waiting: the host runs its own
                            // approval timeout and sends `SessionEnd` when it
                            // expires, and QUIC still notices a connection that
                            // is genuinely gone.
                            let awaiting = writer_state.status() == GuestStatus::AwaitingApproval;
                            let silent = !awaiting && {
                                let health = writer_state.health.lock().unwrap();
                                health.last_message.is_some_and(|at| at.elapsed() > HOST_SILENCE_LIMIT)
                            };
                            if silent {
                                warn!("No word from the host in {HOST_SILENCE_LIMIT:?} — ending the session");
                                break;
                            }

                            // Re-read the path each tick: hole punching can
                            // succeed after the session starts, and a session
                            // that moves off the relay should say so.
                            if let Some(path) = client.connection_path() {
                                let mut health = writer_state.health.lock().unwrap();
                                // Transport RTT beats the application-level
                                // ping: it excludes our own scheduling.
                                if path.rtt_ms() > 0.0 {
                                    health.rtt_ms = path.rtt_ms();
                                }
                                let relayed = !path.is_direct();
                                let tunnelled = path.is_direct() && path.looks_tunnelled();
                                let summary = format!(
                                    "{} to {}, {:.0} ms round trip{}",
                                    path.label(),
                                    path.remote(),
                                    path.rtt_ms(),
                                    if tunnelled {
                                        " — that is a private address, so the packets are \
                                         going through a tunnel and the round trip is the \
                                         tunnel's rather than the distance's"
                                    } else {
                                        ""
                                    },
                                );
                                health.path = Some(path);
                                drop(health);

                                // Said once, and only after hole punching has
                                // had time to work — see the note where the path
                                // is first read.
                                //
                                // # Logged whatever the answer is
                                //
                                // This used to speak up only when the path was
                                // relayed, which left the case people actually
                                // ask about silent: "direct" with a round trip
                                // far too long for the distance. That is what
                                // two machines on a VPN look like — a direct
                                // path to each other's tunnel addresses — and
                                // the log said nothing, so the first
                                // investigation of it had no evidence at all
                                // and the session it concerned had already been
                                // overwritten.
                                if !reported_path && started.elapsed() >= DIRECT_GRACE {
                                    reported_path = true;
                                    if relayed || tunnelled {
                                        warn!("Connection path: {summary}");
                                        if let Some(path) = client.connection_path()
                                            && relayed
                                        {
                                            warn!("{}", path.advice());
                                        }
                                    } else {
                                        info!("Connection path: {summary}");
                                    }
                                }
                            }

                            // The window may have been resized or put
                            // fullscreen since the last tick, which changes what
                            // is worth sending by a factor of four.
                            let current = *writer_state.viewport.lock().unwrap();
                            if current != reported_viewport && current.is_some() {
                                reported_viewport = current;
                                if write_stream_message(
                                    &mut send,
                                    &Message::Config(viewport_config(current)),
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }

                            // The ping carries its own send time, so the pong
                            // gives the round trip without keeping state.
                            let report = writer_state.health.lock().unwrap().take_report();
                            if write_stream_message(&mut send, &Message::Ping(now_ms())).await.is_err() {
                                break;
                            }
                            Message::Telemetry(report)
                        }
                    };
                    if let Err(e) = write_stream_message(&mut send, &message).await {
                        debug!("Send failed: {e}");
                        break;
                    }
                }
            } => Ok(()),
            r = reader => r.context("Receive task panicked"),
        };

        // The audio reader outlives the select, so it has to be stopped here or
        // it holds the stream — and the playback device behind it — open past
        // the end of the session.
        if let Some(side_streams) = side_streams {
            side_streams.abort();
        }
        client.shutdown().await.ok();
        outcome
    }
    .await;

    match result {
        Ok(()) => {
            info!("Guest session ended");
            shared.set_status(GuestStatus::Disconnected);
        }
        Err(ref e) => {
            warn!("Guest session failed: {e:#}");
            shared.set_status(GuestStatus::Failed(format!("{e}")));
        }
    }
    *shared.frame.lock().unwrap() = None;
    result
}

/// Frame decoders, one per codec the host might announce.
///
/// Built lazily from the codec named in `SessionStart` — constructing an H.264
/// decoder for a session that turns out to be lossless would be wasted work,
/// and constructing it eagerly would mean failing before we know what we need.
enum FrameDecoder {
    /// Nothing announced yet.
    Pending,
    Raw,
    Delta(DeltaDecoder),
    H264(Box<H264Decoder>),
    /// H.264 on the GPU. Preferred where the machine has one — software decode
    /// of a native-resolution stream is tens of milliseconds a frame, which is
    /// enough on its own to make a session feel broken.
    #[cfg(target_os = "windows")]
    H264Hardware(Box<tain_core::codec_mf_decode::MediaFoundationDecoder>),
    /// Genuine GPU decode, through libavcodec's D3D11VA path.
    ///
    /// Preferred over [`Self::H264Hardware`], whose name is now a misnomer: the
    /// Media Foundation transform accepts a D3D11 device manager and decodes on
    /// the CPU regardless. Measured at 1080p on an 18.3 Mbit/s stream, decode
    /// went from 12.9 ms a frame to 3.7 — 87 fps to 241.
    #[cfg(all(target_os = "windows", db_ffmpeg))]
    H264Ffmpeg(Box<tain_core::decode_ffmpeg::FfmpegDecoder>),
    /// HEVC, which only exists on the D3D11VA path.
    ///
    /// There is no software HEVC decoder in this build, and none is wanted:
    /// software HEVC of a desktop-sized stream is far past a frame budget. The
    /// host only ever *sends* HEVC to a guest that asked for it, and a guest
    /// whose GPU cannot decode it says so by failing here — which ends the
    /// session with a reason rather than showing nothing.
    #[cfg(all(target_os = "windows", db_ffmpeg))]
    Hevc(Box<tain_core::decode_ffmpeg::FfmpegDecoder>),
}

impl FrameDecoder {
    /// The best H.264 decoder this machine has.
    ///
    /// Hardware first, software if there is none. The fallback is not a
    /// formality: plenty of machines expose a hardware *encoder* and no
    /// hardware *decoder*, so the two cannot be assumed to come together.
    fn h264() -> Self {
        // Real GPU decode first. The Media Foundation path below is a software
        // decoder wearing a hardware name, and is only better than OpenH264.
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        match tain_core::decode_ffmpeg::FfmpegDecoder::new() {
            Ok(decoder) => return Self::H264Ffmpeg(Box::new(decoder)),
            Err(e) => info!("No D3D11VA decoder ({e}) — falling back"),
        }
        #[cfg(target_os = "windows")]
        match tain_core::codec_mf_decode::MediaFoundationDecoder::new() {
            Ok(decoder) => return Self::H264Hardware(Box::new(decoder)),
            Err(e) => info!("No Media Foundation decoder ({e}) — decoding on the CPU"),
        }
        // Construction cannot fail in practice, and a decoder that refuses to
        // exist should not stop the session from reporting why.
        match H264Decoder::new() {
            Ok(decoder) => Self::H264(Box::new(decoder)),
            Err(e) => {
                error!("No H.264 decoder available at all: {e}");
                Self::Pending
            }
        }
    }

    fn for_codec(codec: VideoCodec) -> Result<Self> {
        Ok(match codec {
            VideoCodec::Raw => Self::Raw,
            VideoCodec::DeltaZstd => Self::Delta(DeltaDecoder::new()),
            VideoCodec::H264 => Self::h264(),
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            VideoCodec::H265 => {
                let decoder = tain_core::decode_ffmpeg::FfmpegDecoder::for_codec(codec)
                    .context("this machine cannot decode H.265 — ask the host for H.264")?;
                info!("H.265 decoding on the GPU via {}", decoder.name);
                Self::Hevc(Box::new(decoder))
            }
            other => anyhow::bail!("this build has no decoder for {}", other.label()),
        })
    }

    /// Decode one frame payload into something the renderer can upload.
    ///
    /// `Ok(None)` means the payload produced no picture — a parameter set, or a
    /// frame the encoder skipped. That is normal and must not end the session.
    fn decode(
        &mut self,
        data: &[u8],
        width: u16,
        height: u16,
        is_keyframe: bool,
    ) -> Result<Option<FrameBuffer>> {
        let expected = width as usize * height as usize * 4;
        match self {
            Self::Pending => {
                anyhow::bail!("the host sent a frame before saying which codec it uses")
            }
            Self::Raw => Ok(bgra_to_rgba(data, width, height)),
            Self::Delta(decoder) => {
                let bgra = decoder.decode(data, is_keyframe, expected)?;
                Ok(bgra_to_rgba(&bgra, width, height))
            }
            Self::H264(decoder) => Ok(decoder.decode(data)?.map(|picture| FrameBuffer {
                width: picture.width,
                height: picture.height,
                rgba: picture.rgba,
                gpu: None,
            })),
            #[cfg(target_os = "windows")]
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            Self::H264Ffmpeg(decoder) | Self::Hevc(decoder) => {
                let Some(picture) = decoder.decode(data)? else { return Ok(None) };
                // The texture is asked for *after* decoding, so it names the
                // frame that was just produced rather than the one before it.
                let gpu =
                    decoder.last_gpu_picture().map(|frame| (frame.texture, frame.generation));
                Ok(Some(FrameBuffer {
                    width: picture.width,
                    height: picture.height,
                    rgba: picture.rgba,
                    gpu,
                }))
            }
            Self::H264Hardware(decoder) => Ok(decoder.decode(data)?.map(|picture| FrameBuffer {
                width: picture.width,
                height: picture.height,
                rgba: picture.rgba,
                gpu: None,
            })),

        }
    }

    /// Which decoder this is, for a log line that has to explain a freeze.
    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "no codec agreed yet",
            Self::Raw => "raw",
            Self::Delta(_) => "delta-zstd",
            Self::H264(_) => "H.264 on the CPU",
            #[cfg(target_os = "windows")]
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            Self::H264Ffmpeg(_) => "H.264 on the GPU (D3D11VA)",
            Self::H264Hardware(_) => "H.264 via Media Foundation (CPU)",
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            Self::Hevc(_) => "H.265 on the GPU (D3D11VA)",
        }
    }

    /// The decoder's D3D11 device, or zero where there is not one.
    ///
    /// A `usize` because it crosses to the render thread and a raw pointer is
    /// not `Send`. Nothing dereferences it on the way.
    fn gpu_device(&self) -> usize {
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        if let Self::H264Ffmpeg(decoder) | Self::Hevc(decoder) = self {
            return decoder.device_raw() as usize;
        }
        0
    }

    /// Ask the GPU decoders to stop reading frames back.
    ///
    /// Only the D3D11VA decoders can do it; everything else ignores this and
    /// keeps producing pixels, which is what the caller falls back to.
    fn set_present_on_gpu(&mut self, on: bool) {
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        if let Self::H264Ffmpeg(decoder) | Self::Hevc(decoder) = self {
            decoder.set_present_on_gpu(on);
        }
        let _ = on;
    }

    /// Drop inter-frame state after a failure, so the next keyframe recovers.
    fn reset(&mut self) {
        if let Self::Delta(decoder) = self {
            decoder.reset();
        }
        // The H.264 decoder resynchronises on the next IDR by itself; there is
        // no equivalent state to clear.
    }
}

/// Read host messages, converting frames to RGBA for the renderer.
/// What this guest can display, as a `DisplayConfig`.
///
/// `target_fps` and `max_bitrate_kbps` are left at "host decides": the adaptive
/// controller already works those out from measurement, and a guess from here
/// would only fight it.
fn viewport_config(viewport: Option<(u16, u16)>) -> DisplayConfig {
    let (client_width, client_height) = viewport.unwrap_or((0, 0));
    DisplayConfig { target_fps: 0, max_bitrate_kbps: 0, client_width, client_height }
}

/// How often the guest says where its frames went.
const TALLY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Say where the last few seconds of video went.
///
/// Logged at `info` deliberately. This is the line that answers "it is frozen"
/// without another round of guessing, and it is one line every five seconds.
fn report_tally(
    tally: &FrameTally,
    elapsed: std::time::Duration,
    decoder: &FrameDecoder,
    health: &Mutex<StreamHealth>,
) {
    match tally.summary(elapsed.as_secs_f32()) {
        Some(mut line) => {
            // The two costs the host adapts on. Printed here because "Slow
            // decoding" has been driving the frame rate down all session and
            // neither end has ever shown the number behind the verdict.
            {
                let h = health.lock().unwrap();
                line.push_str(&format!(
                    " | decode {:.1} ms, draw {:.1} ms ({})",
                    h.last_decode_ms(),
                    h.last_render_ms(),
                    decoder.label(),
                ));
            }
            // A stream where nothing decodes is not a slow stream, it is a
            // broken one, and it deserves to be said differently.
            if tally.decoded == 0 {
                warn!("{line} — nothing is reaching the screen");
            } else {
                info!("{line}");
            }
        }
        // Silence. The host may be idle, but it may also have stopped, and the
        // difference has cost several rounds of guessing to establish before.
        None => warn!("video: nothing arrived in the last {}s", elapsed.as_secs()),
    }
}

async fn receive_loop<R>(
    recv: &mut R,
    shared: &GuestSessionState,
    ack_tx: mpsc::UnboundedSender<u32>,
    keyframe_tx: mpsc::UnboundedSender<Message>,
    repaint: impl Fn(),
) -> Result<()>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    // The host names the codec in SessionStart; anything unrecognised is a
    // version mismatch, and guessing would render garbage.
    let mut decoder = FrameDecoder::Pending;
    // What `decoder` was built for, so a reader that learns the codec from the
    // session rather than from a message knows whether it already has the right
    // one. Kept beside the decoder rather than derived from it: a method
    // mapping every variant back to a codec is one more thing to keep in step
    // with the variants.
    let mut built_for: Option<VideoCodec> = None;

    let mut tally = FrameTally::default();
    let mut tallied_at = std::time::Instant::now();

    while let Some(data) = read_stream_message(recv).await? {
        shared.health.lock().unwrap().last_message = Some(std::time::Instant::now());

        // Reported from here rather than on a timer, because the interesting
        // failure is silence and this loop still wakes for telemetry and status
        // messages when no video is arriving at all.
        if tallied_at.elapsed() >= TALLY_INTERVAL {
            report_tally(&tally, tallied_at.elapsed(), &decoder, &shared.health);
            tally = FrameTally::default();
            tallied_at = std::time::Instant::now();
        }
        match bincode::deserialize::<Message>(&data)? {
            Message::SessionStart(ss) => {
                info!(
                    "Session #{} started — {}x{} via {} (host: {})",
                    ss.session_id,
                    ss.screen_info.width,
                    ss.screen_info.height,
                    ss.encoder_type,
                    ss.host_name.as_deref().unwrap_or("anonymous")
                );
                let codec = VideoCodec::from_wire_name(&ss.encoder_type).with_context(|| {
                    format!(
                        "host is streaming '{}', which this build cannot decode",
                        ss.encoder_type
                    )
                })?;
                *shared.codec.lock().unwrap() = Some(codec);
                decoder = FrameDecoder::for_codec(codec)?;
                built_for = Some(codec);
                // Published so the render thread can try to open it for sharing.
                shared.gpu_device.store(decoder.gpu_device(), Ordering::Relaxed);
                *shared.remote_size.lock().unwrap() = (ss.screen_info.width, ss.screen_info.height);
                // Sanitized on arrival: the host chose this string, we did not.
                *shared.host_name.lock().unwrap() =
                    ss.host_name.as_deref().and_then(tain_core::sanitize_device_name);
                // SessionStart only arrives once the host has allowed us, so
                // this is where "waiting" ends.
                if shared.status() == GuestStatus::AwaitingApproval {
                    shared.set_status(GuestStatus::Authenticated);
                }
                repaint();
            }
            Message::ControlState(cs) => {
                info!(
                    "Input permissions: mouse={}, keyboard={}",
                    cs.grants.mouse, cs.grants.keyboard
                );
                *shared.grants.lock().unwrap() = cs.grants;
            }
            #[cfg(all(target_os = "windows", db_ffmpeg))]
            Message::Audio(audio) => {
                // Opened on the first block rather than at connect: a host with
                // nothing to send should not make the guest seize a playback
                // device, and the format is not known until audio arrives.
                // Asked once. Without this the failure below repeats for every
                // packet — a hundred times a second on a machine that simply
                // has no playback device or no ffmpeg beside the exe.
                if shared.audio_failed.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let mut output = shared.audio.lock().unwrap();
                if output.is_none() {
                    match tain_core::audio_output::AudioOutput::start(
                        audio.sample_rate,
                        audio.channels as u16,
                        tain_core::LatencyPriority::new(
                            shared.audio_priority.load(Ordering::Relaxed),
                        ),
                    ) {
                        Ok(started) => *output = Some(started),
                        Err(e) => {
                            // Once. A machine with no playback device would
                            // otherwise log this a hundred times a second.
                            warn!("No audio output ({e}) — the session stays silent");
                            *output = None;
                            drop(output);
                            shared.audio_failed.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                    }
                }
                let buffered = output.as_ref().map(|output| {
                    if let Err(e) = output.push(&audio.packets, audio.first_frame, audio.frames)
                    {
                        debug!("Audio block dropped: {e}");
                    }
                    output.buffered_ms()
                });
                // The audio lock is released before the health lock is taken.
                // Holding both at once establishes an ordering that the render
                // pass, which takes them in the other order, could deadlock
                // against.
                drop(output);

                if let Some(buffered) = buffered {
                    // How far sound is running behind picture. Both are stamped
                    // with the host's clock, so the difference is real rather
                    // than an artefact of two machines disagreeing.
                    shared.health.lock().unwrap().note_audio(audio.captured_ms, buffered);
                }
            }
            Message::Cursor(update) => {
                // Shapes are cached by id: Windows re-reports the same handful
                // constantly as the pointer crosses windows, and the host only
                // sends the image when the guest has not seen it.
                let mut cursor = shared.cursor.lock().unwrap();
                if let Some(shape) = update.shape {
                    cursor.shape = Some(shape);
                }
                cursor.position = Some(update.position);
            }
            Message::StreamStatus(status) => {
                let mut health = shared.health.lock().unwrap();
                if health.status.map(|s| s.bottlenecks) != Some(status.bottlenecks) {
                    let named = BottleneckSet::from_bits(status.bottlenecks)
                        .iter()
                        .map(|b| b.label())
                        .collect::<Vec<_>>();
                    if named.is_empty() {
                        info!("Stream is running clean again");
                    } else {
                        info!("Host reports: {}", named.join(", "));
                    }
                }
                health.status = Some(status);
                drop(health);
                repaint();
            }
            Message::Pong { echo, host_ms } => {
                let arrived = now_ms();
                let rtt = arrived.saturating_sub(echo) as f32;
                let mut health = shared.health.lock().unwrap();
                health.rtt_ms = rtt;
                health.note_clock(echo, host_ms, arrived);
            }
            Message::Frame(fm) => {
                // # The stream carrying frames is not the one carrying the
                // announcement
                //
                // This reader may be the one on the video stream, which never
                // sees `SessionStart` or `CodecChanged`. It takes the answer
                // from the session instead, and rebuilds when it changes —
                // which is also how a mid-session codec switch reaches it.
                //
                // Compared every frame because the comparison is two words and
                // getting it wrong means decoding H.265 as H.264: not an error,
                // just a picture made of garbage.
                let agreed = *shared.codec.lock().unwrap();
                if let Some(agreed) = agreed
                    && built_for != Some(agreed)
                {
                    decoder = FrameDecoder::for_codec(agreed)?;
                    built_for = Some(agreed);
                    shared.gpu_device.store(decoder.gpu_device(), Ordering::Relaxed);
                }

                tally.arrived += 1;
                tally.bytes += fm.data.len();
                // Stamped by the host at encode time, read against this
                // machine's clock corrected by the measured offset.
                shared.health.lock().unwrap().note_frame_latency(fm.timestamp_ms, now_ms());
                if fm.is_keyframe {
                    tally.keyframes += 1;
                }
                let started = std::time::Instant::now();
                // Cheap, and it has to be per frame: the render thread decides
                // this after the session is already running.
                decoder.set_present_on_gpu(shared.present_on_gpu.load(Ordering::Relaxed));
                let decoded = decoder.decode(&fm.data, fm.width, fm.height, fm.is_keyframe);
                shared
                    .health
                    .lock()
                    .unwrap()
                    .record_decode(started.elapsed().as_secs_f32() * 1000.0, fm.data.len());

                // Ack **every** frame taken off the wire, whatever came of it.
                //
                // The ack means "I have consumed this", not "I displayed this",
                // and conflating the two deadlocks the stream. A decoder
                // routinely returns no picture for a payload — it is buffering,
                // or the access unit is incomplete — and a failed decode is
                // exactly when the host most needs to keep sending, because only
                // its next keyframe can recover. Acking solely on success left
                // the host's window full of frames that would never be
                // acknowledged, so it stopped sending until the staleness escape
                // fired seconds later, over and over. That is a slideshow, and
                // it looks identical to a slow network from both ends.
                let _ = ack_tx.send(fm.sequence_number);

                let buffer =
                    match decoded {
                        Ok(Some(buffer)) => {
                            tally.decoded += 1;
                            buffer
                        }
                        // No picture from this payload — normal, keep reading.
                        // Counted rather than ignored: routine in ones and twos,
                        // and a frozen picture when it is every frame.
                        Ok(None) => {
                            tally.no_picture += 1;
                            continue;
                        }
                        Err(e) => {
                            // A failed frame desyncs everything after it, so
                            // nothing is drawn until one arrives that stands
                            // alone — and one is asked for rather than waited
                            // for. The host no longer sends them on a timer;
                            // this is the event they exist to answer, and the
                            // only side that can see it happen.
                            tally.failed += 1;
                            warn!("Frame #{} failed to decode: {e}", fm.sequence_number);
                            decoder.reset();
                            let _ = keyframe_tx.send(Message::RequestKeyframe);
                            continue;
                        }
                    };

                // The decoder is the authority on size: a lossy codec pads to
                // whole macroblocks, and the header is only the host's claim.
                *shared.remote_size.lock().unwrap() =
                    (buffer.width as u16, buffer.height as u16);
                // Replaces any frame the renderer hasn't picked up yet: showing
                // the newest frame beats draining a backlog of stale ones.
                //
                // Counted, though. Replacing one means a frame was captured,
                // encoded, carried, decoded and then dropped one step from the
                // screen — work every stage paid for and nobody saw. It is also
                // what a hitch looks like from the inside, and until now it was
                // the only loss in the pipeline that nothing measured.
                let displaced = shared.frame.lock().unwrap().replace(buffer).is_some();
                if displaced {
                    shared.health.lock().unwrap().note_skipped();
                }
                if shared.status() != GuestStatus::Streaming {
                    shared.set_status(GuestStatus::Streaming);
                }
                repaint();
            }
            Message::HostPreview(picture) => {
                // Validated on arrival: this comes from another machine, and a
                // wrong-sized buffer reaching the image code is a panic rather
                // than a bad picture.
                if picture.len() == tain_core::preview::BYTES {
                    *shared.host_preview.lock().unwrap() = Some(picture);
                } else {
                    debug!("Ignored a host preview of {} bytes", picture.len());
                }
            }
            Message::CodecChanged(change) => {
                // The host switched mid-session. It sends this *before* the
                // first frame of the new codec, so rebuilding here means no
                // frame ever reaches a decoder that cannot read it.
                match VideoCodec::from_wire_name(&change.encoder_type) {
                    Some(codec) => {
                        info!("Host switched to {} — rebuilding the decoder", codec.label());
                        *shared.codec.lock().unwrap() = Some(codec);
                        decoder = FrameDecoder::for_codec(codec)?;
                        built_for = Some(codec);
                        shared.gpu_device.store(decoder.gpu_device(), Ordering::Relaxed);
                        // A new decoder has no reference frames at all, and the
                        // host's next frame is only a keyframe by luck.
                        let _ = keyframe_tx.send(Message::RequestKeyframe);
                    }
                    None => warn!("Host announced a codec this build does not know: {}",
                        change.encoder_type),
                }
            }
            Message::GuestApprovalRequest(gar) => {
                info!("Waiting for the host to allow this connection: {}", gar.label);
                shared.set_status(GuestStatus::AwaitingApproval);
                repaint();
            }
            Message::SessionEnd(se) => {
                info!("Host ended the session: {}", se.reason);
                anyhow::bail!("{}", explain_session_end(&se.reason));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Turn the host's machine-readable reason into something worth reading.
///
/// `invalid_passcode` is the common one and the least self-explanatory: it is
/// what a saved connection hits once the host has regenerated its code, which
/// happens on every host restart (codes are in-memory) and on disconnect if
/// guest autoreset is on. "Failed: invalid_passcode" sends people hunting for a
/// bug that isn't there.
fn explain_session_end(reason: &str) -> String {
    match reason {
        "invalid_passcode" => {
            "that code is no longer valid — the host has generated a new one".to_string()
        }
        other => other.to_string(),
    }
}

/// Convert a BGRA frame payload to RGBA, or `None` if it is the wrong size.
fn bgra_to_rgba(data: &[u8], width: u16, height: u16) -> Option<FrameBuffer> {
    let (w, h) = (width as usize, height as usize);
    let needed = w.checked_mul(h)?.checked_mul(4)?;
    if needed == 0 || data.len() < needed {
        return None;
    }

    let mut rgba = vec![0u8; needed];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(data.chunks_exact(4)) {
        dst[0] = src[2]; // R
        dst[1] = src[1]; // G
        dst[2] = src[0]; // B
        dst[3] = src[3]; // A
    }
    Some(FrameBuffer { width: w, height: h, rgba, gpu: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_channels_are_swapped_into_rgba() {
        // One pixel: B=1, G=2, R=3, A=4.
        let frame = bgra_to_rgba(&[1, 2, 3, 4], 1, 1).unwrap();
        assert_eq!(frame.rgba, vec![3, 2, 1, 4]);
        assert_eq!((frame.width, frame.height), (1, 1));
    }

    #[test]
    fn undersized_payload_is_rejected_rather_than_panicking() {
        // Claims 4x4 but carries one pixel — must not index out of bounds.
        assert!(bgra_to_rgba(&[1, 2, 3, 4], 4, 4).is_none());
        assert!(bgra_to_rgba(&[], 1, 1).is_none());
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        assert!(bgra_to_rgba(&[1, 2, 3, 4], 0, 4).is_none());
        assert!(bgra_to_rgba(&[1, 2, 3, 4], 4, 0).is_none());
    }

    #[test]
    fn oversized_payload_is_truncated_to_the_stated_size() {
        let data = vec![9u8; 64];
        let frame = bgra_to_rgba(&data, 2, 2).unwrap();
        assert_eq!(frame.rgba.len(), 2 * 2 * 4);
    }

    #[test]
    fn status_labels_distinguish_every_state() {
        assert_eq!(GuestStatus::Disconnected.label(), "Disconnected");
        assert!(GuestStatus::Failed("nope".into()).label().contains("nope"));
        assert!(!GuestStatus::Disconnected.is_active());
        assert!(GuestStatus::Connecting.is_active());
        assert!(GuestStatus::Streaming.is_active());
        assert!(!GuestStatus::Failed("x".into()).is_active());
    }

    #[test]
    fn waiting_for_approval_is_a_live_state_with_its_own_wording() {
        // It is neither "connecting" nor "connected": the connection is up and
        // the delay is someone else's decision, which the label has to say.
        let waiting = GuestStatus::AwaitingApproval;
        assert!(waiting.is_active());
        assert!(waiting.label().to_lowercase().contains("host"));
        assert_ne!(waiting.label(), GuestStatus::Connecting.label());
    }

    /// A reader that never hears the announcement still has to decode.
    ///
    /// The regression this pins: frames moved to their own stream, and the loop
    /// reading that stream never sees `SessionStart`, where the codec is named.
    /// Every frame was refused — eight a second arriving, none decoded, a
    /// session stuck on "waiting for video" with the reason in the log all
    /// along. The codec belongs to the session rather than to whichever reader
    /// happened to hear about it.
    #[tokio::test]
    async fn a_reader_takes_the_codec_from_the_session_when_it_never_hears_it() {
        use tain_core::protocol::{FrameMessage, Message};

        let shared = GuestSessionState::new();
        // What the control stream would have learned and written down.
        *shared.codec.lock().unwrap() = Some(VideoCodec::Raw);

        // One 2x2 frame, and nothing else — no `SessionStart` anywhere.
        let (mut host, mut guest) = tokio::io::duplex(64 * 1024);
        let frame = Message::Frame(FrameMessage {
            sequence_number: 1,
            width: 2,
            height: 2,
            timestamp_ms: 0,
            is_keyframe: true,
            data: vec![0u8; 2 * 2 * 4],
        });
        tain_core::network::write_stream_message(&mut host, &frame).await.unwrap();
        drop(host);

        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<u32>();
        let (control_tx, _control_rx) = mpsc::unbounded_channel::<Message>();
        receive_loop(&mut guest, &shared, ack_tx, control_tx, || {}).await.unwrap();

        // Acked means decoded: the loop refuses a frame it cannot read.
        assert_eq!(ack_rx.try_recv().ok(), Some(1), "the frame should have been decoded");
        assert!(shared.frame.lock().unwrap().is_some(), "and handed to the renderer");
    }

    #[test]
    fn a_frame_arriving_before_the_codec_is_announced_is_an_error_not_a_guess() {
        let mut decoder = FrameDecoder::Pending;
        let err = decoder
            .decode(&[0; 16], 2, 2, true)
            .err()
            .expect("guessing the codec would render garbage");
        assert!(err.to_string().contains("codec"));
    }

    /// The clock offset must come out right, and with the right sign.
    ///
    /// A sign error here does not fail — it produces a latency figure that
    /// looks plausible and is wrong by twice the clock difference, which is
    /// exactly the kind of number someone would then try to optimise against.
    #[test]
    fn the_host_clock_offset_is_recovered_from_a_round_trip() {
        let mut health = StreamHealth::default();

        // Host clock runs 5000 ms ahead. Ping leaves at 1000 (our clock), host
        // answers at its 6050 (= our 1050), reply lands at our 1100.
        health.note_clock(1000, 6050, 1100);
        let offset = health.clock_offset_ms.expect("an exchange must produce an estimate");
        assert!(
            (offset - 5000.0).abs() < 1.0,
            "offset came out {offset}, expected about +5000"
        );

        // A frame the host stamped at 6000 was captured at our 1000, so a frame
        // arriving now at 1080 is 80 ms old — not 5080 and not -4920.
        health.note_frame_latency(6000, 1080);
        let recorded = health.latency_ms.last().copied().expect("a sample");
        assert!((recorded - 80.0).abs() < 2.0, "latency came out {recorded}, expected about 80");
    }

    /// A clock behind ours must work exactly as well as one ahead.
    #[test]
    fn a_host_clock_behind_ours_is_handled_too() {
        let mut health = StreamHealth::default();
        // Host is 5000 ms behind: ping at 10000, host answers at its 5050.
        health.note_clock(10_000, 5_050, 10_100);
        let offset = health.clock_offset_ms.expect("estimate");
        assert!((offset + 5000.0).abs() < 1.0, "offset came out {offset}, expected about -5000");

        health.note_frame_latency(5_000, 10_080);
        let recorded = health.latency_ms.last().copied().expect("a sample");
        assert!((recorded - 80.0).abs() < 2.0, "latency came out {recorded}");
    }

    /// Nothing is recorded before the offset is known.
    ///
    /// Two unsynchronised clocks subtract to the disagreement between them, not
    /// a latency, and a session that opened with "lat 4903 ms" would be read as
    /// the app being broken.
    #[test]
    fn no_latency_is_reported_until_the_clocks_have_been_compared() {
        let mut health = StreamHealth::default();
        health.note_frame_latency(1_700_000_000_000, 1_699_999_000_000);
        assert!(health.latency_ms.is_empty(), "reported a latency with no clock estimate");
    }

    /// An implausible sample is dropped rather than shown.
    #[test]
    fn an_absurd_sample_is_discarded() {
        let mut health = StreamHealth::default();
        health.note_clock(1000, 1050, 1100);

        // A frame stamped in the future: the estimate has not settled, or a
        // clock stepped underneath us.
        health.note_frame_latency(9_000, 1_100);
        assert!(health.latency_ms.is_empty(), "kept a negative latency");

        // And one impossibly old.
        health.note_frame_latency(1_000, 900_000);
        assert!(health.latency_ms.is_empty(), "kept a multi-second latency");
    }

    #[test]
    fn every_available_codec_has_a_decoder() {
        for codec in VideoCodec::ALL.into_iter().filter(|c| c.is_available()) {
            assert!(
                FrameDecoder::for_codec(codec).is_ok(),
                "{codec:?} is selectable on the host but undecodable here"
            );
        }
    }

    #[test]
    fn raw_frames_go_straight_through_the_decoder() {
        let mut decoder = FrameDecoder::for_codec(VideoCodec::Raw).unwrap();
        // One BGRA pixel: B=1, G=2, R=3, A=4.
        let frame = decoder.decode(&[1, 2, 3, 4], 1, 1, true).unwrap().unwrap();
        assert_eq!(frame.rgba, vec![3, 2, 1, 4]);
    }

    #[test]
    fn an_h264_payload_with_no_picture_yet_is_not_an_error() {
        let mut decoder = FrameDecoder::for_codec(VideoCodec::H264).unwrap();
        // An empty payload is what a skipped frame looks like on the wire.
        assert!(decoder.decode(&[], 64, 64, false).unwrap().is_none());
    }

    #[test]
    fn a_stale_saved_code_says_so_instead_of_showing_a_wire_constant() {
        let explained = explain_session_end("invalid_passcode");
        assert!(!explained.contains('_'), "still reads like a wire constant: {explained}");
        assert!(explained.contains("no longer valid"));

        // Reasons written for humans pass straight through.
        let human = "the host did not allow this connection";
        assert_eq!(explain_session_end(human), human);
    }

    #[test]
    fn resetting_a_stateless_decoder_is_harmless() {
        // Only the delta path carries state, but the caller resets whatever it
        // has after any decode failure.
        for codec in [VideoCodec::Raw, VideoCodec::H264, VideoCodec::DeltaZstd] {
            FrameDecoder::for_codec(codec).unwrap().reset();
        }
        FrameDecoder::Pending.reset();
    }

    /// Full guest path against a real host: connect from a code, authenticate,
    /// receive frames, and land them in the state the renderer reads.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires network access to the n0 relay"]
    async fn guest_session_streams_frames_from_a_real_host() {
        use tain_core::{CaptureConfig, EncodeConfig, HostServer, QualityScale, SessionConfig};
        use std::time::Duration;

        let config = SessionConfig {
            capture: CaptureConfig {
                fps: 30,
                quality: QualityScale::Native,
                show_cursor: false,
                native_override: Some((64, 36)),
            },
            encode: EncodeConfig::default(),
            approval_timeout: Duration::from_secs(5),
            auto_regen_guest_passcode: false,
            exclude_audio_processes: Vec::new(),
            audio_bitrate_kbps: 160,
            host_name: Some("Test Host".into()),
            priority: Default::default(),
        };

        let server = Arc::new(HostServer::bind().await.unwrap().with_config(config));
        let code_string = server.generate_master_passcode().await.unwrap();
        tokio::spawn({
            let server = Arc::clone(&server);
            async move { server.run().await }
        });

        let code = ConnectionCode::parse(&code_string).unwrap();
        let state = GuestSessionState::new();
        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let session = tokio::spawn(run_guest_session(
            code,
            // Nothing remembered in a test: the harness connects to a host it
            // just started, so there is no previous address to reuse.
            Vec::new(),
            Some("Test Guest".into()),
            state.clone(),
            input_rx,
            || {},
        ));

        let frame = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                if let Some(frame) = state.frame.lock().unwrap().take() {
                    return frame;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("no frame arrived from the host");

        assert_eq!((frame.width, frame.height), (64, 36));
        assert_eq!(frame.rgba.len(), 64 * 36 * 4);
        assert_eq!(state.status(), GuestStatus::Streaming);
        assert_eq!(*state.remote_size.lock().unwrap(), (64, 36));
        // A master code grants control immediately, and the host announces it.
        assert_eq!(*state.grants.lock().unwrap(), ControlGrants::ALL);
        // And the host's name reaches us, which is what the saved list files
        // the connection under.
        assert_eq!(state.host_name.lock().unwrap().as_deref(), Some("Test Host"));

        session.abort();
        server.shutdown().await.ok();
    }

    #[test]
    fn reset_clears_frame_grants_and_host_name() {
        let state = GuestSessionState::new();
        *state.frame.lock().unwrap() = Some(FrameBuffer { width: 1, height: 1, rgba: vec![0; 4], gpu: None });
        *state.grants.lock().unwrap() = ControlGrants::ALL;
        *state.status.lock().unwrap() = GuestStatus::Streaming;
        *state.host_name.lock().unwrap() = Some("Studio".into());

        state.reset();

        assert_eq!(state.status(), GuestStatus::Disconnected);
        assert!(state.frame.lock().unwrap().is_none());
        assert_eq!(*state.grants.lock().unwrap(), ControlGrants::NONE);
        assert!(state.host_name.lock().unwrap().is_none());
    }
}
