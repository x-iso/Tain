//! One guest session, and everything the UI needs to drive it.
//!
//! Sessions live in their own window rather than the main one (see
//! [`super::session_window`]). Each is independent: its own connection, texture,
//! input state, and scaling. That is what makes several concurrent sessions
//! possible, and it means fullscreen belongs to the session window rather than
//! to the app as a whole.

use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tracing::*;

use tain_core::protocol::{InputEvent, KeyEvent, MouseButtonEvent};
use tain_core::{ConnectionCode, ControlGrants};

use super::keymap::ModifierTracker;
use super::network_bridge::{run_guest_session, GuestSessionState, GuestStatus};
use super::settings::{ScalingMode, Settings};

/// Distinguishes tabs even when two sessions target the same host.
static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

/// Fraction of the viewport, measured from each edge, where the pointer starts
/// pulling the view in that direction.
pub const PAN_MARGIN: f32 = 0.2;

/// Pan speed in source pixels per second, at the very edge of the window.
///
/// Scales linearly from zero at the inner boundary of the margin, so a slow
/// approach nudges and a hard shove into the corner travels quickly.
const PAN_SPEED: f32 = 1400.0;

/// A live (or failed) connection to one host.
pub struct GuestSession {
    pub tab_id: u64,
    /// Short label for the tab, derived from the host's endpoint.
    pub label: String,
    /// Hex of the host's endpoint, keying this session to a saved connection.
    pub endpoint_hex: String,
    pub shared: GuestSessionState,
    /// Scaling for this session only, seeded from the saved default.
    pub scaling: ScalingMode,
    /// This session points at our own host endpoint, so input must not be
    /// forwarded — injection is real, and it would drive our own pointer.
    pub loopback: bool,
    /// Which part of a 1:1 view is on screen, in source pixels from the
    /// top-left. Meaningless in `Fit`, and clamped when drawn.
    pub pan: egui::Vec2,
    /// Whether this end wants to send pointer and keyboard input right now.
    ///
    /// # Two different questions
    ///
    /// The host decides whether this machine *may* send input; these decide
    /// whether it currently *wants* to. Turning one off is how you watch a
    /// session without risk of typing into it, and it can never widen what the
    /// host allowed — [`Self::sends_mouse`] and [`Self::sends_keyboard`] take
    /// both into account.
    ///
    /// Seeded from the host's grants when the session opens, so the toolbar
    /// starts by matching what was permitted rather than asking twice.
    pub want_mouse: bool,
    pub want_keyboard: bool,
    /// Whether to play the host's audio. Preference only until the audio
    /// transport lands; see the audio tasks.
    pub want_sound: bool,
    /// Playback gain for the host's audio, 0.0 to 1.0.
    ///
    /// Per session rather than global: the point of it is that the remote
    /// machine can be quieter than whatever this one is already playing, and
    /// two open sessions are two different remote machines.
    pub volume: f32,
    /// Whether this session ever reached the streaming state.
    ///
    /// The difference between "the host hung up on us" and "we never got in".
    /// The first closes its own tab; the second has to stay open, because the
    /// reason it failed is the only thing on screen worth reading.
    pub was_streaming: bool,
    /// Whether to draw the host's pointer over the video.
    ///
    /// On by default — not seeing where the remote pointer is makes a session
    /// hard to use. Worth turning off when watching rather than driving, since
    /// two pointers on one screen is confusing.
    pub show_remote_cursor: bool,
    /// Whether the grants have been read once to seed the toggles above.
    grants_seeded: bool,

    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    input_tx: Option<mpsc::UnboundedSender<InputEvent>>,
    texture: Option<egui::TextureHandle>,
    /// The decoded frame still on the GPU when it was never read back, and the
    /// size to draw it at.
    ///
    /// Holding the picture is what keeps its surface alive: this outlives the
    /// decoder whenever a session ends while a frame is on screen, which is
    /// most of the time. See `tain_core::decode_ffmpeg::GpuPicture`.
    #[cfg(target_os = "windows")]
    gpu_frame: Option<(tain_core::decode_ffmpeg::GpuPicture, egui::Vec2)>,
    /// Interop state for this session, built once the decoder has a device.
    ///
    /// `Some(None)` means it was tried and this machine cannot — checked once so
    /// a driver that refuses is not asked again every frame.
    #[cfg(target_os = "windows")]
    renderer: Option<Option<super::gl_interop::SharedRenderer>>,
    /// Which D3D11 device [`Self::renderer`] was opened against.
    ///
    /// # Why this is not "the decoder's device" once and for all
    ///
    /// `wglDXOpenDeviceNV` shares one device, and every texture registered
    /// afterwards has to belong to *that* device. The session decides its codec
    /// in stages, and each stage builds a fresh decoder with a fresh device —
    /// `for_codec` is called from three places, and a log of a working session
    /// shows two decoders inside a third of a second.
    ///
    /// Whether the render thread opens the interop before or after the last of
    /// them is a race, and it is the difference between a session that runs and
    /// one that does not. Losing it means registering textures on a device that
    /// never owned them: one machine repeated its first few frames, the other
    /// died a few seconds in.
    ///
    /// So the device is remembered and compared. It cannot be inferred from the
    /// renderer, which only knows the handle it opened, and it must not be
    /// assumed stable, which is what the old comment here did.
    #[cfg(target_os = "windows")]
    renderer_device: usize,
    /// Whether this session may draw frames straight from the GPU at all.
    ///
    /// The user's choice, taken when the session opened. See
    /// `Settings::gpu_present` for why it is a choice.
    gpu_present: bool,
    /// Frames in a row that arrived with neither pixels nor a texture.
    ///
    /// See [`Session::upload_pending_frame`], where going back to the readback
    /// is decided.
    #[cfg(target_os = "windows")]
    empty_frames: u32,
    last_pointer: Option<(u16, u16)>,
    buttons_down: [bool; 3],
    /// Which of the wire's contact slots each live finger is using.
    ///
    /// egui identifies a contact with an arbitrary 64-bit number, and the wire
    /// carries a byte — because the host's injector holds its contacts in an
    /// array of a size it declared up front. So the mapping is made here, at
    /// the only place that sees a finger arrive and leave.
    ///
    /// The lowest free slot is taken on `Start` and returned on `End`, which
    /// keeps a two-finger gesture on slots 0 and 1 no matter how many fingers
    /// have been and gone before it.
    touch_slots: [Option<u64>; tain_core::protocol::MAX_TOUCH_CONTACTS as usize],
    modifiers: ModifierTracker,
    keys_down: Vec<u16>,
    /// The most recent picture of this host, and when it was taken.
    ///
    /// Sampled from a frame that was already decoded on the processor, so it
    /// costs a downscale and nothing else. See [`super::preview`].
    preview: Option<Vec<u8>>,
    preview_at: Option<std::time::Instant>,
}

impl GuestSession {
    /// Dial the host a code addresses and start streaming.
    pub fn start(
        code: ConnectionCode,
        known_addrs: Vec<std::net::SocketAddr>,
        settings: &Settings,
        loopback: bool,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = GuestSessionState::new();
        let (input_tx, input_rx) = mpsc::unbounded_channel();

        // First bytes of the endpoint: enough to tell two hosts apart without
        // putting a secret in a tab title.
        let label = format!(
            "{}…{}",
            if loopback { "loopback " } else { "" },
            hex_prefix(&code.endpoint_id)
        );
        let endpoint_hex = super::connections::endpoint_hex(&code.endpoint_id);

        // The name we publish is ours, resolved now rather than at connect time
        // so a settings change mid-session doesn't rewrite what we already said.
        let client_name = settings.device_name();
        // Published before the session starts, because playback opens on the
        // first audio block and reads it then.
        shared
            .audio_priority
            .store(settings.priority().value(), Ordering::Relaxed);
        let task = tokio::spawn(run_guest_session(
            code,
            known_addrs,
            client_name,
            shared.clone(),
            input_rx,
            repaint,
        ));

        Self {
            tab_id: NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed),
            label,
            endpoint_hex,
            shared,
            scaling: settings.scaling_mode,
            loopback,
            pan: egui::Vec2::ZERO,
            // Off until the host's grants arrive and seed them; see
            // `seed_input_toggles`. Starting on would mean a moment where input
            // is armed before anyone has said it may be.
            want_mouse: false,
            want_keyboard: false,
            want_sound: true,
            volume: 1.0,
            was_streaming: false,
            show_remote_cursor: true,
            grants_seeded: false,
            task: Some(task),
            input_tx: Some(input_tx),
            texture: None,
            #[cfg(target_os = "windows")]
            gpu_frame: None,
            #[cfg(target_os = "windows")]
            renderer: None,
            #[cfg(target_os = "windows")]
            renderer_device: 0,
            gpu_present: settings.gpu_present,
            #[cfg(target_os = "windows")]
            empty_frames: 0,
            last_pointer: None,
            buttons_down: [false; 3],
            touch_slots: [None; tain_core::protocol::MAX_TOUCH_CONTACTS as usize],
            modifiers: ModifierTracker::default(),
            keys_down: Vec::new(),
            preview: None,
            preview_at: None,
        }
    }

    /// Switch scaling mode, resetting the pan so 1:1 always opens at the
    /// top-left rather than wherever a previous 1:1 session left off.
    pub fn set_scaling(&mut self, mode: ScalingMode) {
        if self.scaling != mode {
            self.scaling = mode;
            self.pan = egui::Vec2::ZERO;
        }
    }

    /// Slide the 1:1 view when the pointer nears an edge.
    ///
    /// Only meaningful when the remote screen is bigger than the window; on an
    /// axis that already fits, `pan_limit` is zero and this does nothing.
    pub fn auto_pan(&mut self, viewport: egui::Rect, src: egui::Vec2, pointer: Option<egui::Pos2>, dt: f32) {
        if self.scaling != ScalingMode::Actual {
            return;
        }
        let Some(pointer) = pointer else { return };
        if !viewport.contains(pointer) {
            return;
        }

        let limit = super::theme::pan_limit(viewport.size(), src);
        let delta = egui::vec2(
            pan_velocity(pointer.x - viewport.left(), viewport.width()),
            pan_velocity(pointer.y - viewport.top(), viewport.height()),
        ) * PAN_SPEED
            * dt;

        self.pan = egui::vec2(
            (self.pan.x + delta.x).clamp(0.0, limit.x),
            (self.pan.y + delta.y).clamp(0.0, limit.y),
        );
    }

    pub fn status(&self) -> GuestStatus {
        self.shared.status()
    }

    pub fn is_active(&self) -> bool {
        self.status().is_active()
    }

    pub fn grants(&self) -> ControlGrants {
        *self.shared.grants.lock().unwrap()
    }

    /// A session with no network behind it, for testing pure decision logic.
    ///
    /// Deliberately `cfg(test)`: nothing in the application should be able to
    /// make a session that is not connected to anything.
    #[cfg(test)]
    pub(crate) fn test_stub() -> Self {
        Self {
            tab_id: 0,
            label: "test".into(),
            endpoint_hex: String::new(),
            shared: GuestSessionState::default(),
            scaling: ScalingMode::default(),
            loopback: false,
            pan: egui::Vec2::ZERO,
            want_mouse: false,
            want_keyboard: false,
            want_sound: true,
            volume: 1.0,
            was_streaming: false,
            show_remote_cursor: true,
            grants_seeded: false,
            task: None,
            input_tx: None,
            texture: None,
            #[cfg(target_os = "windows")]
            gpu_frame: None,
            #[cfg(target_os = "windows")]
            renderer: None,
            #[cfg(target_os = "windows")]
            renderer_device: 0,
            gpu_present: false,
            #[cfg(target_os = "windows")]
            empty_frames: 0,
            last_pointer: None,
            buttons_down: [false; 3],
            touch_slots: [None; tain_core::protocol::MAX_TOUCH_CONTACTS as usize],
            modifiers: ModifierTracker::default(),
            keys_down: Vec::new(),
            preview: None,
            preview_at: None,
        }
    }

    #[cfg(test)]
    fn set_test_grants(&mut self, mouse: bool, keyboard: bool) {
        *self.shared.grants.lock().unwrap() = ControlGrants { mouse, keyboard };
    }

    /// Push the sound toggle down to the playback stream.
    ///
    /// Muting rather than closing the device: the timeline keeps running, so
    /// unmuting resumes at the live position instead of replaying whatever
    /// accumulated while it was off.
    pub fn apply_sound_preference(&self) {
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        if let Some(output) = self.shared.audio.lock().unwrap().as_ref() {
            output.set_muted(!self.want_sound);
            output.set_volume(self.volume);
        }
    }

    /// How much audio the jitter buffer is aiming to hold, for the stats row.
    ///
    /// Worth showing next to the buffered figure: on a link that keeps breaking
    /// the buffer this is where the audio delay went, and it is the difference
    /// between "the sound is late" and "the sound is late *because it had to
    /// be*".
    pub fn audio_target_ms(&self) -> Option<u32> {
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        {
            return self.shared.audio.lock().unwrap().as_ref().map(|o| o.target_ms());
        }
        #[allow(unreachable_code)]
        None
    }

    /// Milliseconds of audio buffered, for the stats row.
    pub fn audio_buffered_ms(&self) -> Option<u32> {
        #[cfg(all(target_os = "windows", db_ffmpeg))]
        {
            return self.shared.audio.lock().unwrap().as_ref().map(|o| o.buffered_ms());
        }
        #[allow(unreachable_code)]
        None
    }

    /// The host's pointer position and its uploaded image.
    ///
    /// The texture is rebuilt only when the shape id changes — Windows reports
    /// the same handful of pointers constantly as the mouse crosses windows,
    /// and re-uploading one per frame would cost more than the video.
    pub fn cursor_texture(
        &mut self,
        ctx: &egui::Context,
    ) -> Option<(tain_core::cursor::CursorPosition, egui::TextureHandle)> {
        let mut cursor = self.shared.cursor.lock().unwrap();
        let position = cursor.position?;
        let shape = cursor.shape.as_ref()?;

        let needs_upload = !matches!(&cursor.texture, Some((id, _)) if *id == shape.id);
        if needs_upload {
            let size = [shape.width as usize, shape.height as usize];
            if size[0] == 0 || size[1] == 0 || shape.rgba.len() < size[0] * size[1] * 4 {
                return None;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied(size, &shape.rgba);
            let id = shape.id;
            let handle = ctx.load_texture(
                "remote-cursor",
                image,
                // Nearest, not linear: a pointer is a small hard-edged glyph and
                // filtering it makes it look smeared rather than smooth.
                egui::TextureOptions::NEAREST,
            );
            cursor.texture = Some((id, handle));
        }
        let handle = cursor.texture.as_ref().map(|(_, h)| h.clone())?;
        Some((position, handle))
    }

    /// The current pointer's hotspot, in source pixels.
    pub fn cursor_hotspot(&self) -> egui::Vec2 {
        let cursor = self.shared.cursor.lock().unwrap();
        cursor
            .shape
            .as_ref()
            .map(|s| egui::vec2(s.hotspot_x as f32, s.hotspot_y as f32))
            .unwrap_or_default()
    }

    /// Whether pointer input should actually be sent.
    ///
    /// The host's grant is the ceiling; the local toggle can only narrow it.
    pub fn sends_mouse(&self) -> bool {
        self.want_mouse && self.grants().mouse && !self.loopback
    }

    /// Whether keyboard input should actually be sent.
    pub fn sends_keyboard(&self) -> bool {
        self.want_keyboard && self.grants().keyboard && !self.loopback
    }

    /// Match the toggles to the host's grants, once, when they first arrive.
    ///
    /// Only the first time: after that the toggles are the user's, and a grant
    /// arriving again — the host re-approving, say — must not silently switch
    /// input back on for someone who deliberately turned it off.
    pub fn seed_input_toggles(&mut self) {
        if self.grants_seeded {
            return;
        }
        let grants = self.grants();
        if grants.mouse || grants.keyboard {
            self.want_mouse = grants.mouse;
            self.want_keyboard = grants.keyboard;
            self.grants_seeded = true;
        }
    }

    /// Record how large the video is being drawn, for the host to cap against.
    ///
    /// Rounded up to even numbers and given a floor: H.264 needs even
    /// dimensions, and a window dragged to nothing must not ask for a 0x0
    /// stream on the way past.
    /// # Why the scaling mode decides whether there is a ceiling at all
    ///
    /// `Fit` draws the whole picture inside the window, so anything the host
    /// sends beyond the window's own pixels is encoded, transmitted and thrown
    /// away on arrival — the window is a real ceiling and saying so saves all
    /// of that.
    ///
    /// `1:1` means the opposite: show the host's pixels at their own size and
    /// pan around what does not fit. Capping to the window there is
    /// self-defeating — the guest pans across an image that was downscaled
    /// before it was sent, so "actual size" shows no more detail than Fit did
    /// and the mode does nothing. It is also what makes "all screens at 1:1"
    /// mean full resolution rather than a wall squeezed into one window.
    pub fn report_viewport(&self, size: egui::Vec2, pixels_per_point: f32) {
        let wanted = match self.scaling {
            ScalingMode::Actual => None,
            ScalingMode::Fit => {
                let to_px =
                    |v: f32| ((v * pixels_per_point).round().max(2.0) as u32).min(u16::MAX as u32);
                let (w, h) = (to_px(size.x) as u16, to_px(size.y) as u16);
                Some(((w + 1) & !1, (h + 1) & !1))
            }
        };
        let mut slot = self.shared.viewport.lock().unwrap();
        if *slot != wanted {
            *slot = wanted;
        }
    }

    pub fn remote_size(&self) -> (u16, u16) {
        *self.shared.remote_size.lock().unwrap()
    }

    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        self.texture.as_ref()
    }

    /// Take or release the GPU frame, telling the decoder which one it is.
    ///
    /// Assignment goes through here so it cannot be done without publishing.
    /// The decoder needs the answer to know when a rebuilt converter's textures
    /// are safe to free, and the cost of a missed publish is the crash this
    /// exists to prevent — see `FfmpegDecoder::release_pictures_except`.
    #[cfg(target_os = "windows")]
    fn hold_gpu_frame(
        &mut self,
        frame: Option<(tain_core::decode_ffmpeg::GpuPicture, egui::Vec2)>,
    ) {
        // Published after the field is set, so there is no moment where the
        // decoder is told a generation is in use before it actually is. The
        // reverse order would be harmless too — the generation being taken up
        // is always the current converter's, which is never a retired one.
        let generation = frame.as_ref().map_or(0, |(picture, _)| picture.generation);
        let texture = frame.as_ref().map_or(0, |(picture, _)| picture.texture);
        self.gpu_frame = frame;
        self.shared
            .drawing_generation
            .store(generation, std::sync::atomic::Ordering::Relaxed);
        // Which of the converter's targets this is, so the decoder rotates
        // around it rather than through it.
        self.shared
            .drawing_texture
            .store(texture, std::sync::atomic::Ordering::Relaxed);
    }

    /// The frame to draw straight from the GPU, if there is one.
    #[cfg(target_os = "windows")]
    pub fn gpu_frame(&self) -> Option<(usize, egui::Vec2)> {
        // Only meaningful once there is something able to draw it.
        self.renderer.as_ref()?.as_ref()?;
        self.gpu_frame.as_ref().map(|(picture, size)| (picture.texture, *size))
    }

    /// A paint callback drawing this session's current frame over `rect`.
    #[cfg(target_os = "windows")]
    pub fn gpu_paint(&self, rect: egui::Rect) -> Option<egui::PaintCallback> {
        let renderer = self.renderer.as_ref()?.as_ref()?.clone();
        let (picture, _) = self.gpu_frame.as_ref()?;
        Some(super::gl_interop::paint_callback(renderer, picture.clone(), rect))
    }

    /// Open the decoder's device for sharing with OpenGL, and reopen it if the
    /// decoder is replaced by one on a different device.
    ///
    /// Runs on the render thread, which is the only place the GL context is
    /// current. A refusal is recorded against the device that refused, so it is
    /// not retried per frame but is asked again if the decoder changes — a
    /// driver that would not share one device has said nothing about another.
    #[cfg(target_os = "windows")]
    fn ensure_renderer(&mut self) {
        use std::sync::atomic::Ordering;

        if !self.gpu_present {
            // Recorded as "asked and answered", so this does not run again.
            self.renderer = Some(None);
            return;
        }
        let device = self.shared.gpu_device.load(Ordering::Relaxed);
        if device == 0 {
            // No GPU decoder yet, or none on this machine. Ask again next frame.
            return;
        }
        if self.renderer.is_some() && self.renderer_device == device {
            return;
        }

        if self.renderer.is_some() {
            tracing::info!(
                "The decoder moved to another device — reopening the GPU path against it"
            );
            // Both belong to the device being left behind: the frame is a
            // texture on it, and the renderer holds registrations against it.
            // Neither may outlive this, and the frame goes first because the
            // renderer is what is drawing it.
            self.hold_gpu_frame(None);
            self.renderer = None;
        }

        // SAFETY: this is the render thread with the context current, and the
        // pointer is the device the decoder published, which is kept alive by
        // the interop's own reference for as long as it is open.
        let renderer = unsafe { super::gl_interop::renderer_for(device as *mut _) };
        // Told before the first GPU frame is expected, so the decoder stops
        // reading back from the next one onwards.
        self.shared.present_on_gpu.store(renderer.is_some(), Ordering::Relaxed);
        self.renderer = Some(renderer);
        self.renderer_device = device;
    }

    /// Upload a newly arrived frame, if there is one.
    ///
    /// `take`-ing means a repaint between frames costs nothing, and reusing the
    /// handle avoids reallocating a GPU texture every frame.
    pub fn upload_pending_frame(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "windows")]
        self.ensure_renderer();

        let Some(frame) = self.shared.frame.lock().unwrap().take() else { return };

        // A frame that never left the GPU has nothing to upload. Recorded and
        // drawn from where it already is.
        #[cfg(target_os = "windows")]
        if let Some(picture) = frame.gpu {
            self.hold_gpu_frame(Some((
                picture,
                egui::vec2(frame.width as f32, frame.height as f32),
            )));
            self.shared
                .health
                .lock()
                .unwrap()
                .record_render(0.0);
            #[cfg(target_os = "windows")]
            {
                self.empty_frames = 0;
            }
            return;
        }
        #[cfg(target_os = "windows")]
        {
            self.hold_gpu_frame(None);
        }

        // A frame with neither pixels nor a texture. It happens in the window
        // between the decoder being told to stop reading back and the renderer
        // being ready — and `from_rgba_premultiplied` panics on a short buffer
        // rather than returning anything, so this cannot be left to chance.
        if frame.rgba.len() < frame.width * frame.height * 4 {
            debug!("Dropped a frame with no pixels and no texture");
            // # A window that does not always close
            //
            // Dropping these was written for the frame or two in flight while
            // the decoder learns it may stop reading back. On a guest where the
            // texture never arrives it is every frame from then on, and the
            // picture stops at whatever was on screen when the switch happened:
            // reported as a frozen first frame, with skip-the-readback on at
            // one end, and it stays frozen because nothing ever reconsiders.
            //
            // Only the libavcodec D3D11VA decoders honour the request at all —
            // the Media Foundation one ignores it and keeps producing pixels —
            // so which decoder a guest ends up with decides whether the setting
            // does anything, and neither end can see that from the other side.
            //
            // Rather than work out every way a texture can fail to appear, take
            // the absence as the answer: ask for the readback again, and let
            // the session carry on with a hiccup instead of a freeze.
            #[cfg(target_os = "windows")]
            {
                /// Frames of grace before giving up. A couple can legitimately
                /// be in flight; a tenth of a second of them cannot.
                const PATIENCE: u32 = 8;

                self.empty_frames += 1;
                if self.empty_frames == PATIENCE
                    && self.shared.present_on_gpu.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        "This decoder is not handing over its textures — going back to \
                         reading frames back to the processor. Drawing straight from the \
                         GPU is off for this session."
                    );
                }
            }
            return;
        }
        #[cfg(target_os = "windows")]
        {
            self.empty_frames = 0;
        }
        // A picture for this host's card, taken from a frame that is already
        // here and already decoded. Rarely, because nothing about it is urgent
        // and the cost belongs to the video otherwise.
        const PREVIEW_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
        if self.preview_at.is_none_or(|at| at.elapsed() >= PREVIEW_EVERY) {
            self.preview_at = Some(std::time::Instant::now());
            if let Some(picture) =
                tain_core::preview::from_rgba(&frame.rgba, frame.width, frame.height)
            {
                self.preview = Some(picture);
            }
        }

        // Timed because this is the one stage neither end can otherwise see. A
        // machine that decodes fine but cannot get the pixels onto its own
        // screen looks, from every other measurement, like a healthy stream.
        let started = std::time::Instant::now();

        // `from_rgba_unmultiplied` runs a branching, lookup-table conversion per
        // pixel — 4.1 million of them at 1600p, on the UI thread, every frame.
        // For an opaque picture it is the identity: `Color32` is `[r, g, b, a]`
        // and the unmultiplied path returns exactly that when alpha is 255.
        //
        // Every frame from every decoder here is opaque — the desktop has no
        // transparency and the converters all write 255 — so the premultiplied
        // constructor produces identical pixels for a straight move instead of a
        // per-pixel call. Measured at 8.5 ms/frame before, which at 60 fps is
        // half the budget spent doing nothing.
        debug_assert!(
            frame.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "a frame with real alpha would decode differently through this path"
        );
        let image =
            egui::ColorImage::from_rgba_premultiplied([frame.width, frame.height], &frame.rgba);
        match self.texture {
            Some(ref mut tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture(
                    format!("remote-{}", self.tab_id),
                    image,
                    egui::TextureOptions::LINEAR,
                ))
            }
        }

        self.shared
            .health
            .lock()
            .unwrap()
            .record_render(started.elapsed().as_secs_f32() * 1000.0);
    }

    /// Stages the host says are currently holding this stream back.
    pub fn bottlenecks(&self) -> tain_core::BottleneckSet {
        self.shared.health.lock().unwrap().bottlenecks()
    }

    /// The live numbers, for the toolbar.
    ///
    /// Deliberately what is *measured* rather than what was asked for: a session
    /// that is unusable while the host believes it is sending 60 fps is exactly
    /// the case these exist to make visible.
    pub fn live_stats(&self) -> String {
        let health = self.shared.health.lock().unwrap();
        let live = health.live;

        // # Anchored, so the eye can stay in one place
        //
        // These are read while something is going wrong, which means they are
        // read by glancing at the same spot repeatedly. Built the obvious way —
        // join whatever happens to apply — every field moved whenever any field
        // to its left changed width, and a figure that jumps as it updates is
        // one that has to be re-found every second instead of watched.
        //
        // So: the four that are always worth having come first, at fixed
        // widths, and anything conditional is appended after them where it can
        // shift nothing. Rendered in a monospaced font, which is what makes a
        // fixed width in characters a fixed width on screen.
        //
        // Two different latencies, and the difference between them is the
        // point: `lat` is what the user feels — capture on the host through to
        // arrival here, pipeline included — while `net` is only the round trip.
        // A large gap between them says the delay is in the encoder or the
        // decoder rather than the link, which is the first thing anyone
        // diagnosing a sluggish session needs to know.
        let mut parts = vec![
            format!("{:>3.0} fps", live.displayed_fps),
            match live.latency_ms > 0.0 {
                true => format!("lat {:>4.0} ms", live.latency_ms + live.render_ms),
                // Holds the column open rather than closing it and moving
                // everything after it once the first measurement lands.
                false => "lat    — ms".to_string(),
            },
            format!("net {:>4.0} ms", health.rtt_ms),
            match live.received_kbps >= 1000.0 {
                // Both units are six characters, so the switch between them
                // does not move anything either.
                true => format!("{:>6.1} Mbit/s", live.received_kbps / 1000.0),
                false => format!("{:>6.0} kbit/s", live.received_kbps),
            },
        ];

        // Frames that arrived and were never drawn get their own figure rather
        // than being folded into the rate. "58 fps" and "58 fps, 12 skipped" are
        // the same number and completely different situations: the second one is
        // a hitch every eighty milliseconds that nothing else here would show.
        if live.skipped_fps >= 1.0 {
            parts.push(format!("+{:.0} skipped", live.skipped_fps));
        }
        // Only when it is worth knowing. Under about 30 ms nobody perceives an
        // audio/video offset, and a number that is always there is noise.
        if live.av_offset_ms.abs() >= 30.0 {
            let behind = if live.av_offset_ms > 0.0 { "sound behind" } else { "sound ahead" };
            parts.push(format!("{behind} {:.0} ms", live.av_offset_ms.abs()));
        }
        parts.join("  ·  ")
    }

    /// How the connection is carried, when that is worth saying.
    ///
    /// Returns `None` for a healthy direct path — it is the expected case and
    /// does not need a chip. A relay does: it is the single biggest determinant
    /// of whether a session is usable, and it is otherwise completely invisible.
    /// The first real two-machine test of this app ran over a relay for minutes
    /// while the picture crawled, and nothing on screen said so.
    ///
    /// A session can no longer *begin* on a relay — see
    /// `tain_core::network::await_direct_path`, which refuses one rather than
    /// letting it start. So this now means something narrower and worse: a
    /// session that had a direct path and lost it, and is being carried by the
    /// relay until one comes back.
    pub fn relay_warning(&self) -> Option<String> {
        let health = self.shared.health.lock().unwrap();
        match health.path {
            Some(ref path) if !path.is_direct() => Some(path.advice()),
            _ => None,
        }
    }

    /// Where the connection graph should be drawn, failures included.
    pub fn progress(&self) -> Option<super::progress::Progress> {
        super::progress::resting(&self.status(), self.shared.furthest())
    }

    /// What each stage of the connection produced.
    pub fn outcomes(&self) -> super::progress::Outcomes {
        let health = self.shared.health.lock().unwrap();
        super::progress::Outcomes {
            found_via: health.found_via,
            // What separates arriving on the code's relay from arriving on a
            // remembered address, when no lookup answered. See `Outcomes`.
            on_relay: health.path.as_ref().is_some_and(|path| !path.is_direct()),
            peer: health.path.as_ref().map(|path| path.remote().to_string()),
            // The transport round trip rather than the application ping: it is
            // the property of the path that the `Direct` stage produced.
            rtt_ms: health.path.as_ref().map(|path| path.rtt_ms()),
            fps: Some(health.live.displayed_fps),
        }
    }

    /// The whole path from "Connect" to a moving picture, written out.
    ///
    /// What the session window shows on hover in place of the single word the
    /// header has room for.
    pub fn path_summary(&self) -> String {
        super::progress::summary(self.progress(), &self.outcomes())
    }

    /// Every display this session may see, as the host described them.
    ///
    /// Empty until `SessionStart`, and from a host that cannot enumerate its
    /// displays — which means "one screen, the one being streamed", not none.
    pub fn displays(&self) -> Vec<tain_core::protocol::DisplayInfo> {
        self.shared.displays.lock().unwrap().clone()
    }

    /// What is being streamed, once the host has said.
    pub fn watching(&self) -> Option<tain_core::protocol::DisplayChoice> {
        *self.shared.watching.lock().unwrap()
    }

    /// Ask the host to show a different display.
    ///
    /// Fire and forget: the host answers with the display it is *actually*
    /// showing, which is what updates [`Self::watching`]. Nothing here assumes
    /// the request will be honoured, because a monitor can be unplugged between
    /// the menu being drawn and the click landing.
    pub fn watch_display(&self, choice: tain_core::protocol::DisplayChoice) {
        let control = self.shared.control.lock().unwrap();
        match control.as_ref() {
            Some(tx) => {
                let _ = tx.send(tain_core::protocol::Message::WatchDisplay(choice));
            }
            None => debug!("No session to ask for {choice:?}"),
        }
    }

    /// Which address lookup located this host, once one has.
    pub fn found_via(&self) -> Option<tain_core::discovery::Branch> {
        self.shared.health.lock().unwrap().found_via
    }


    /// The longer breakdown, shown on hover.
    pub fn health_detail(&self) -> String {
        let health = self.shared.health.lock().unwrap();
        let live = health.live;
        let mut lines = vec![
            format!("Decoding {:.1} ms per frame", live.decode_ms),
            format!("Drawing {:.1} ms per frame", live.render_ms),
        ];
        match health.status {
            Some(status) => lines.push(format!(
                "Host is sending {} fps at up to {} kbit/s{}",
                status.fps,
                status.bitrate_kbps,
                if health.is_degraded() { ", reduced from what it was asked for" } else { "" }
            )),
            None => lines.push("The host has not reported its settings yet.".into()),
        }
        if live.skipped_fps >= 1.0 {
            lines.push(format!(
                "{:.0} frames a second arrived and were replaced before this window drew them",
                live.skipped_fps
            ));
        }

        // # Why the address is spelled out
        //
        // "direct" only means the connection is not going through an iroh
        // relay. It does not mean the packets take the short route. Two
        // machines both on a VPN hole-punch to their *tunnel* addresses, and
        // the round trip that follows is the tunnel's — which is how a session
        // between two neighbouring cities reads as 110 ms with nothing on
        // screen to account for it.
        if let Some(ref path) = health.path {
            lines.push(format!(
                "{} path to {}, {:.0} ms round trip",
                path.label(),
                path.remote(),
                path.rtt_ms()
            ));
            if path.is_direct() && path.looks_tunnelled() {
                lines.push(
                    "That is a private address, so this is going through a VPN or \
                     similar tunnel unless the other machine is on your own network. \
                     The round trip above is the tunnel's, not the internet's — worth \
                     comparing with the VPN off."
                        .into(),
                );
            }
        }

        lines.join("
")
    }

    pub fn send_input(&self, event: InputEvent) {
        // Input injection is real. Sending to our own endpoint would move this
        // machine's pointer, which then moves the pointer we are reading from.
        if self.loopback {
            return;
        }
        if let Some(ref tx) = self.input_tx {
            let _ = tx.send(event);
        }
    }

    pub fn modifiers_mut(&mut self) -> &mut ModifierTracker {
        &mut self.modifiers
    }

    /// The picture to remember this host by.
    ///
    /// The host's own, when it sent one. It is better in every way: it arrives
    /// on connection rather than ten seconds in, it is blurred before it goes
    /// on the wire, and it exists even when this end never sees a frame on the
    /// processor — which is the case for a decoder drawing straight from the
    /// GPU. The locally sampled one is the fallback for hosts too old to send
    /// theirs.
    pub fn preview(&self) -> Option<Vec<u8>> {
        if let Some(from_host) = self.shared.host_preview.lock().unwrap().clone() {
            return Some(from_host);
        }
        self.preview.clone()
    }

    /// Where this host answered, once a direct path has been established.
    ///
    /// `None` while the session is relayed, which is the case worth *not*
    /// remembering: a relay's address is the relay's, and dialling it next time
    /// would reach a server that has never heard of this host.
    pub fn direct_addr(&self) -> Option<String> {
        let health = self.shared.health.lock().unwrap();
        let path = health.path.as_ref()?;
        path.is_direct().then(|| path.remote().to_string())
    }

    pub fn last_pointer(&self) -> Option<(u16, u16)> {
        self.last_pointer
    }

    pub fn set_last_pointer(&mut self, point: (u16, u16)) {
        self.last_pointer = Some(point);
    }

    /// The wire slot for a finger that is down, allocating one if it is new.
    ///
    /// `None` when every slot is taken — an eleventh finger, which the host
    /// could not inject anyway.
    pub fn touch_slot(&mut self, id: u64) -> Option<u8> {
        if let Some(slot) = self.touch_slots.iter().position(|s| *s == Some(id)) {
            return Some(slot as u8);
        }
        let free = self.touch_slots.iter().position(Option::is_none)?;
        self.touch_slots[free] = Some(id);
        Some(free as u8)
    }

    /// The slot a finger already holds, without handing out a new one.
    ///
    /// For movement, which must not start a gesture: a contact that began
    /// outside the video was never forwarded, and its first `Move` inside is
    /// not a press.
    pub fn touch_slot_if_live(&self, id: u64) -> Option<u8> {
        self.touch_slots.iter().position(|s| *s == Some(id)).map(|slot| slot as u8)
    }

    /// Give a slot back, returning the one that was in use.
    ///
    /// `None` for a finger that was never allocated one, which is a lift for a
    /// touch that started somewhere it was not being forwarded from.
    pub fn release_touch_slot(&mut self, id: u64) -> Option<u8> {
        let slot = self.touch_slots.iter().position(|s| *s == Some(id))?;
        self.touch_slots[slot] = None;
        Some(slot as u8)
    }

    /// Every slot currently held, and clear them.
    ///
    /// For the cases where forwarding stops with fingers still down — the host
    /// withdrawing the mouse grant, the pointer moving onto the toolbar. The
    /// caller has to tell the host those contacts are gone, or they stay
    /// pressed against its screen with nothing left to lift them.
    pub fn take_touch_slots(&mut self) -> Vec<u8> {
        let mut held = Vec::new();
        for (slot, id) in self.touch_slots.iter_mut().enumerate() {
            if id.take().is_some() {
                held.push(slot as u8);
            }
        }
        held
    }

    pub fn button_state(&self, index: usize) -> bool {
        self.buttons_down[index]
    }

    pub fn set_button_state(&mut self, index: usize, down: bool) {
        self.buttons_down[index] = down;
    }

    /// Track a key the host now believes is held.
    ///
    /// Every key that came from the immersive hook, not only the Win keys it
    /// used to be: the hook is the whole keyboard while it is armed, so it is
    /// also the only thing that knows what is still down when it comes off.
    pub fn note_held_key(&mut self, keycode: u16, pressed: bool) {
        if pressed {
            if !self.keys_down.contains(&keycode) {
                self.keys_down.push(keycode);
            }
        } else {
            self.keys_down.retain(|k| *k != keycode);
        }
    }

    /// Release every key the host thinks is held.
    ///
    /// Leaving one down makes the host behave as though it were stuck, with
    /// nothing local able to clear it — and a stuck Ctrl or Alt is worse than a
    /// stuck letter, because it changes what every later keystroke means.
    pub fn release_held_keys(&mut self) {
        for keycode in std::mem::take(&mut self.keys_down) {
            self.send_input(InputEvent::Key(KeyEvent {
                keycode,
                pressed: false,
                modifiers: 0,
                // A release synthesised here, not observed — there is no
                // hardware event to take a scancode from.
                scancode: 0,
                extended: false,
            }));
        }
    }

    /// Tear the session down, releasing anything held on the host.
    pub fn stop(&mut self) {
        for event in self.modifiers.release_all() {
            self.send_input(InputEvent::Key(event));
        }
        self.release_held_keys();
        for index in 0..self.buttons_down.len() {
            if self.buttons_down[index] {
                self.send_input(InputEvent::MouseButton(MouseButtonEvent {
                    button: index as u8,
                    pressed: false,
                }));
            }
        }
        self.buttons_down = [false; 3];

        // # Before the task, not after
        //
        // The interop device was opened against the decoder's `ID3D11Device`
        // and holds GL textures registered against the decoder's own textures.
        // Aborting the task first drops the decoder, and everything the
        // teardown then unregisters and closes belongs to something that is
        // already gone — an access violation, and the reported crash on
        // disconnecting with skip-the-readback enabled.
        //
        // Released here instead, while the decoder is still alive, on the
        // thread that owns the GL context. `Drop` keeps its own guard for the
        // paths that do not come through here, such as the window closing.
        #[cfg(target_os = "windows")]
        {
            self.hold_gpu_frame(None);
            self.renderer = None;
            self.shared.present_on_gpu.store(false, std::sync::atomic::Ordering::Relaxed);
        }

        // # Closed, not killed
        //
        // Dropping the input channel is what tells the session to wind up: the
        // writer's loop ends the moment it closes, and on the way out it tells
        // the host the session is over and closes the connection properly.
        //
        // Aborting the task took that away. Nothing was sent and nothing was
        // closed — the log said `Endpoint dropped without calling
        // Endpoint::close` — so the host went on showing a guest that had left
        // until QUIC timed the connection out tens of seconds later.
        //
        // The handle is dropped rather than awaited, which detaches it: this
        // runs on the interface thread and must not block it. What bounds the
        // task is its own goodbye timeout and the silence limit it watches.
        self.input_tx = None;
        drop(self.task.take());
        self.texture = None;
        self.last_pointer = None;
        self.touch_slots = [None; tain_core::protocol::MAX_TOUCH_CONTACTS as usize];
        self.shared.reset();
        debug!("Session tab {} closed", self.tab_id);
    }
}

impl Drop for GuestSession {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.stop();
        }
    }
}

/// First four bytes of an endpoint, hex encoded.
fn hex_prefix(endpoint: &[u8; 32]) -> String {
    endpoint[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// How hard the view should be pulled along one axis, in the range -1.0..=1.0.
///
/// Zero anywhere in the middle 60% of the window, then ramping up linearly to
/// full speed at either edge. A dead zone in the middle is what stops the view
/// drifting while you are simply using the remote machine.
fn pan_velocity(offset: f32, extent: f32) -> f32 {
    if extent <= 0.0 {
        return 0.0;
    }
    let margin = extent * PAN_MARGIN;
    if margin <= 0.0 {
        return 0.0;
    }
    if offset < margin {
        // Near the left/top edge: pull the view back towards the origin.
        -((margin - offset) / margin).clamp(0.0, 1.0)
    } else if offset > extent - margin {
        ((offset - (extent - margin)) / margin).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    /// A decoder that never hands over a texture must not freeze the picture.
    ///
    /// The reported fault: skip-the-readback on at one end, a frozen first
    /// frame, and nothing that ever reconsiders. The decoder has been told to
    /// stop reading back, so its frames carry no pixels, and it produces no
    /// texture either — every frame from then on is dropped.
    #[test]
    #[cfg(target_os = "windows")]
    fn a_decoder_that_never_produces_a_texture_goes_back_to_the_readback() {
        use std::sync::atomic::Ordering;

        let ctx = egui::Context::default();
        let mut session = super::GuestSession::test_stub();
        session.shared.present_on_gpu.store(true, Ordering::Relaxed);

        // Eight frames carrying neither pixels nor a texture, which is what a
        // decoder that ignores the request looks like from here.
        for _ in 0..8 {
            *session.shared.frame.lock().unwrap() = Some(crate::gui::network_bridge::FrameBuffer {
                width: 320,
                height: 180,
                rgba: Vec::new(),
                gpu: None,
            });
            session.upload_pending_frame(&ctx);
        }

        assert!(
            !session.shared.present_on_gpu.load(Ordering::Relaxed),
            "the session should have asked for the readback back rather than dropping every frame for the rest of its life",
        );
    }

    /// And a working GPU path is left alone.
    ///
    /// The frame carries a real one-pixel texture rather than an invented
    /// address, because a `GpuPicture` owns the surface it names and there is
    /// deliberately no way to make one that does not. That is the whole point of
    /// the type: an address with nothing keeping it alive is what crashed a
    /// guest at disconnect.
    #[test]
    #[cfg(target_os = "windows")]
    fn frames_that_carry_a_texture_keep_the_gpu_path() {
        use std::sync::atomic::Ordering;

        let Some(picture) = one_pixel_picture() else {
            eprintln!("no D3D11 device — skipping");
            return;
        };

        let ctx = egui::Context::default();
        let mut session = super::GuestSession::test_stub();
        session.shared.present_on_gpu.store(true, Ordering::Relaxed);

        for _ in 0..8 {
            *session.shared.frame.lock().unwrap() = Some(crate::gui::network_bridge::FrameBuffer {
                width: 320,
                height: 180,
                rgba: Vec::new(),
                gpu: Some(picture.clone()),
            });
            session.upload_pending_frame(&ctx);
        }

        assert!(session.shared.present_on_gpu.load(Ordering::Relaxed));
    }

    /// A decoder that moves to another device takes the GPU path with it.
    ///
    /// # The race this lost
    ///
    /// `wglDXOpenDeviceNV` shares one device and every texture registered
    /// afterwards has to belong to it. The session settles its codec in stages
    /// and each stage builds a decoder with a device of its own — two inside a
    /// third of a second, in the logs of both a working session and a crashed
    /// one. The only difference between them was whether the render thread
    /// opened the interop before or after the last decoder appeared.
    ///
    /// Opening once and never looking again meant registering textures on a
    /// device that never owned them. One machine repeated its first few frames;
    /// the other died a few seconds in.
    ///
    /// `renderer_for` gives up at its `OnceLock` when there is no render thread,
    /// so no GL or D3D call is reached here and the handles below are never
    /// dereferenced. What is under test is the bookkeeping that decides whether
    /// they would be.
    #[test]
    #[cfg(target_os = "windows")]
    fn a_decoder_on_a_new_device_reopens_the_gpu_path() {
        use std::sync::atomic::Ordering;

        let mut session = super::GuestSession::test_stub();
        session.gpu_present = true;

        // The decoder built while the codec was still being agreed.
        session.shared.gpu_device.store(0x1000, Ordering::Relaxed);
        session.ensure_renderer();
        assert_eq!(session.renderer_device, 0x1000);
        assert!(session.renderer.is_some(), "the attempt is recorded either way");

        // A frame from that device, on screen.
        if let Some(picture) = one_pixel_picture() {
            session.hold_gpu_frame(Some((picture, egui::vec2(1.0, 1.0))));
            assert!(session.gpu_frame.is_some(), "the frame should be held to begin with");
        }

        // The codec settles and the decoder is rebuilt on a device of its own.
        session.shared.gpu_device.store(0x2000, Ordering::Relaxed);
        session.ensure_renderer();

        assert_eq!(
            session.renderer_device, 0x2000,
            "the interop is still bound to the device the decoder has left, so every \
             texture registered from now on belongs to a device that never owned it"
        );
        assert!(
            session.gpu_frame.is_none(),
            "the frame on screen is a texture on the old device and must not be drawn again"
        );

        // And a decoder that stays put is not disturbed.
        session.shared.gpu_device.store(0x2000, Ordering::Relaxed);
        session.ensure_renderer();
        assert_eq!(session.renderer_device, 0x2000);
    }

    /// A `GpuPicture` over a real one-pixel surface, or `None` without a GPU.
    #[cfg(target_os = "windows")]
    fn one_pixel_picture() -> Option<tain_core::decode_ffmpeg::GpuPicture> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG,
            D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };

        let mut device = None;
        // SAFETY: out-parameters are checked; nothing else is borrowed.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
        }
        let device = device?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: the descriptor is initialised; the out-parameter is checked.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)).ok()? };
        Some(tain_core::decode_ffmpeg::GpuPicture::from_texture(texture?, 1, 1, 1))
    }

    /// A local toggle narrows what the host allowed. It can never widen it.
    ///
    /// The failure this prevents is not cosmetic: if the toggle alone decided,
    /// a guest could send input to a host that had refused it, and the refusal
    /// would exist only as a greyed-out icon.
    #[test]
    fn a_local_toggle_cannot_send_input_the_host_refused() {
        let mut session = GuestSession::test_stub();

        // Host allows nothing. Wanting it changes nothing.
        session.set_test_grants(false, false);
        session.want_mouse = true;
        session.want_keyboard = true;
        assert!(!session.sends_mouse(), "pointer must not be sent without a grant");
        assert!(!session.sends_keyboard(), "keys must not be sent without a grant");

        // Host allows both; the local toggles now decide.
        session.set_test_grants(true, true);
        assert!(session.sends_mouse());
        assert!(session.sends_keyboard());
        session.want_keyboard = false;
        assert!(session.sends_mouse());
        assert!(!session.sends_keyboard(), "the local toggle must still be able to refuse");

        // A loopback session forwards nothing whatever either side says.
        session.loopback = true;
        session.want_keyboard = true;
        assert!(!session.sends_mouse());
        assert!(!session.sends_keyboard());
    }

    /// The toggles start matching the host, then stop tracking it.
    #[test]
    fn seeding_happens_once_so_a_deliberate_refusal_is_not_undone() {
        let mut session = GuestSession::test_stub();
        session.set_test_grants(true, true);
        session.seed_input_toggles();
        assert!(session.want_mouse && session.want_keyboard, "seeded from the grants");

        // The user turns typing off, then the host re-approves the session.
        session.want_keyboard = false;
        session.set_test_grants(true, true);
        session.seed_input_toggles();
        assert!(!session.want_keyboard, "re-approval must not switch input back on");
    }

    /// Seeding waits for grants rather than latching on an empty set.
    #[test]
    fn seeding_waits_until_the_host_has_actually_granted_something() {
        let mut session = GuestSession::test_stub();
        session.set_test_grants(false, false);
        session.seed_input_toggles();
        // Nothing granted yet, so nothing has been decided.
        session.set_test_grants(true, true);
        session.seed_input_toggles();
        assert!(session.want_mouse, "grants arriving late must still seed the toggles");
    }

    /// Two fingers must keep the two lowest slots, however many have been
    /// and gone before them.
    ///
    /// The host holds its contacts in an array sized once, up front. Slots that
    /// climbed with every touch would walk off the end of it during any long
    /// session on a touchscreen.
    #[test]
    fn contact_slots_are_reused_rather_than_climbing() {
        let mut session = GuestSession::test_stub();

        for finger in 0..40u64 {
            assert_eq!(session.touch_slot(finger), Some(0), "a lone finger is slot zero");
            assert_eq!(session.release_touch_slot(finger), Some(0));
        }

        // Two at once take the two lowest, and the lower is freed first.
        assert_eq!(session.touch_slot(100), Some(0));
        assert_eq!(session.touch_slot(101), Some(1));
        assert_eq!(session.release_touch_slot(100), Some(0));
        assert_eq!(session.touch_slot(102), Some(0), "the freed slot is the next one out");
    }

    /// A finger already down keeps its slot rather than taking another.
    #[test]
    fn asking_twice_for_a_live_contact_gives_the_same_slot() {
        let mut session = GuestSession::test_stub();
        let first = session.touch_slot(7).expect("a free slot");
        assert_eq!(session.touch_slot(7), Some(first));
        assert_eq!(session.touch_slot_if_live(7), Some(first));
        assert_eq!(session.touch_slot_if_live(8), None, "movement must not start a gesture");
    }

    /// More fingers than the host can inject are refused, not wrapped around.
    #[test]
    fn contacts_beyond_the_hosts_capacity_are_refused() {
        let mut session = GuestSession::test_stub();
        let capacity = tain_core::protocol::MAX_TOUCH_CONTACTS as u64;
        for finger in 0..capacity {
            assert!(session.touch_slot(finger).is_some(), "finger {finger} should fit");
        }
        assert_eq!(session.touch_slot(capacity), None, "one finger too many gets no slot");
    }

    /// Every contact has to be lift-able when forwarding stops mid-gesture.
    #[test]
    fn taking_the_held_slots_reports_them_all_and_leaves_none() {
        let mut session = GuestSession::test_stub();
        for finger in 0..3u64 {
            session.touch_slot(finger);
        }
        assert_eq!(session.take_touch_slots(), vec![0, 1, 2]);
        assert!(session.take_touch_slots().is_empty(), "a second call has nothing left to lift");
        // And the slots are genuinely free again.
        assert_eq!(session.touch_slot(99), Some(0));
    }

    use super::*;
    use tain_core::PasscodeType;

    fn code(endpoint: [u8; 32]) -> ConnectionCode {
        ConnectionCode::new(PasscodeType::Master, [1u8; 32], endpoint)
    }

    #[test]
    fn labels_distinguish_hosts_without_leaking_the_secret() {
        let a = code([0xAB; 32]);
        let b = code([0xCD; 32]);
        assert_ne!(hex_prefix(&a.endpoint_id), hex_prefix(&b.endpoint_id));
        assert_eq!(hex_prefix(&a.endpoint_id), "abababab");
        // The secret must never appear in a tab title.
        assert!(!hex_prefix(&a.endpoint_id).contains("01"));
    }

    #[tokio::test]
    async fn tab_ids_are_unique_across_sessions() {
        let settings = Settings::default();
        let a = GuestSession::start(code([1; 32]), Vec::new(), &settings, true, || {});
        let b = GuestSession::start(code([1; 32]), Vec::new(), &settings, true, || {});
        assert_ne!(a.tab_id, b.tab_id, "two tabs to the same host must be distinguishable");
    }

    #[tokio::test]
    async fn loopback_sessions_never_forward_input() {
        let settings = Settings::default();
        let mut session = GuestSession::start(code([1; 32]), Vec::new(), &settings, true, || {});
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.input_tx = Some(tx);

        session.send_input(InputEvent::Key(KeyEvent {
            scancode: 0,
            extended: false,
            keycode: 65,
            pressed: true,
            modifiers: 0,
        }));
        assert!(rx.try_recv().is_err(), "no input may reach a loopback session");

        session.loopback = false;
        session.send_input(InputEvent::Key(KeyEvent {
            scancode: 0,
            extended: false,
            keycode: 65,
            pressed: true,
            modifiers: 0,
        }));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn stopping_releases_every_held_input() {
        let settings = Settings::default();
        let mut session = GuestSession::start(code([1; 32]), Vec::new(), &settings, false, || {});
        let (tx, mut rx) = mpsc::unbounded_channel();
        session.input_tx = Some(tx);

        session.note_held_key(0x5B, true);
        session.set_button_state(0, true);
        session.set_button_state(2, true);
        // Left Ctrl held, as the OS would report it.
        let mut down = [false; 6];
        down[2] = true;
        session.modifiers.diff_actual(down, [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5]);

        session.stop();

        let mut key_releases = 0;
        let mut button_releases = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                InputEvent::Key(k) => {
                    assert!(!k.pressed);
                    key_releases += 1;
                }
                InputEvent::MouseButton(b) => {
                    assert!(!b.pressed);
                    button_releases += 1;
                }
                other => panic!("unexpected teardown event: {other:?}"),
            }
        }
        assert_eq!(key_releases, 2, "ctrl and the Win key should be released");
        assert_eq!(button_releases, 2, "both held buttons should be released");
        assert!(!session.is_active());
    }

    // ── Auto-pan ─────────────────────────────────────────────

    fn viewport() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    /// A 1:1 session on a remote screen larger than the window.
    async fn panning_session() -> GuestSession {
        let settings = Settings { scaling_mode: ScalingMode::Actual, ..Settings::default() };
        GuestSession::start(code([1; 32]), Vec::new(), &settings, true, || {})
    }

    #[test]
    fn the_middle_of_the_window_does_not_pan() {
        // The dead zone is what stops the view drifting while you work.
        assert_eq!(pan_velocity(400.0, 800.0), 0.0);
        assert_eq!(pan_velocity(200.0, 800.0), 0.0, "exactly at the margin boundary");
        assert_eq!(pan_velocity(600.0, 800.0), 0.0);
    }

    #[test]
    fn the_pull_ramps_up_towards_each_edge() {
        // 20% of 800 is a 160px margin on each side.
        assert!((pan_velocity(800.0, 800.0) - 1.0).abs() < 1e-6, "full speed at the right edge");
        assert!((pan_velocity(0.0, 800.0) + 1.0).abs() < 1e-6, "full speed at the left edge");
        assert!((pan_velocity(720.0, 800.0) - 0.5).abs() < 1e-6, "half way into the margin");
        assert!(pan_velocity(700.0, 800.0) < pan_velocity(760.0, 800.0), "deeper pulls harder");
        assert_eq!(pan_velocity(400.0, 0.0), 0.0, "a zero-width window cannot pan");
    }

    #[tokio::test]
    async fn approaching_an_edge_slides_the_view_and_stops_at_the_end() {
        let mut session = panning_session().await;
        let src = egui::vec2(1920.0, 1080.0);

        // Pointer hard against the right edge for a tenth of a second.
        session.auto_pan(viewport(), src, Some(egui::pos2(799.0, 300.0)), 0.1);
        assert!(session.pan.x > 0.0, "the view should have moved right");
        assert_eq!(session.pan.y, 0.0, "the vertical middle should not drift");

        // Keep going: it must stop flush with the far edge, not run off.
        for _ in 0..100 {
            session.auto_pan(viewport(), src, Some(egui::pos2(799.0, 300.0)), 0.1);
        }
        assert_eq!(session.pan.x, 1920.0 - 800.0);
    }

    #[tokio::test]
    async fn panning_back_stops_at_the_origin() {
        let mut session = panning_session().await;
        let src = egui::vec2(1920.0, 1080.0);
        session.pan = egui::vec2(400.0, 200.0);

        for _ in 0..100 {
            session.auto_pan(viewport(), src, Some(egui::pos2(1.0, 1.0)), 0.1);
        }
        assert_eq!(session.pan, egui::Vec2::ZERO);
    }

    #[tokio::test]
    async fn fit_mode_never_pans() {
        let settings = Settings { scaling_mode: ScalingMode::Fit, ..Settings::default() };
        let mut session = GuestSession::start(code([1; 32]), Vec::new(), &settings, true, || {});

        session.auto_pan(viewport(), egui::vec2(1920.0, 1080.0), Some(egui::pos2(799.0, 599.0)), 1.0);
        assert_eq!(session.pan, egui::Vec2::ZERO);
    }

    #[tokio::test]
    async fn a_screen_that_already_fits_has_nowhere_to_pan() {
        let mut session = panning_session().await;
        session.auto_pan(viewport(), egui::vec2(320.0, 240.0), Some(egui::pos2(799.0, 599.0)), 1.0);
        assert_eq!(session.pan, egui::Vec2::ZERO);
    }

    #[tokio::test]
    async fn a_pointer_outside_the_window_does_not_pan() {
        // Otherwise the view would keep sliding while you use another window.
        let mut session = panning_session().await;
        session.auto_pan(viewport(), egui::vec2(1920.0, 1080.0), Some(egui::pos2(-40.0, 300.0)), 1.0);
        session.auto_pan(viewport(), egui::vec2(1920.0, 1080.0), None, 1.0);
        assert_eq!(session.pan, egui::Vec2::ZERO);
    }

    #[tokio::test]
    async fn changing_scaling_mode_recentres_the_view() {
        // Coming back to 1:1 should start at the top-left, not wherever the
        // last 1:1 session was left scrolled to.
        let mut session = panning_session().await;
        session.pan = egui::vec2(500.0, 300.0);

        session.set_scaling(ScalingMode::Fit);
        assert_eq!(session.pan, egui::Vec2::ZERO);

        session.pan = egui::vec2(500.0, 300.0);
        session.set_scaling(ScalingMode::Fit);
        assert_eq!(session.pan, egui::vec2(500.0, 300.0), "re-selecting the same mode is a no-op");
    }

    #[tokio::test]
    async fn a_session_knows_which_saved_connection_it_belongs_to() {
        let settings = Settings::default();
        let session = GuestSession::start(code([0xAB; 32]), Vec::new(), &settings, false, || {});
        assert_eq!(session.endpoint_hex, "ab".repeat(32));
    }

    #[tokio::test]
    async fn win_key_tracking_does_not_duplicate() {
        let settings = Settings::default();
        let mut session = GuestSession::start(code([1; 32]), Vec::new(), &settings, false, || {});
        session.note_held_key(0x5B, true);
        session.note_held_key(0x5B, true);
        assert_eq!(session.keys_down.len(), 1);

        session.note_held_key(0x5B, false);
        assert!(session.keys_down.is_empty());
    }
}
