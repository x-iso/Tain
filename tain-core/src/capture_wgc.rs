//! Capturing a display through the compositor instead of duplicating it.
//!
//! # Why there are two ways to capture a screen
//!
//! Desktop Duplication makes the application duplicate an output, and requires
//! the D3D device and that output to be on the same adapter — the one drawing
//! the desktop. On a laptop with two graphics chips that is broken by default
//! and cannot be repaired from inside the process: which chip a process runs on
//! is decided before `main`, and the adapter holding the output is the one that
//! refuses to duplicate it. See [`super::gpu_preference`], which carries the
//! whole diagnosis and the workaround that came before this one.
//!
//! Windows.Graphics.Capture asks DWM for frames instead. DWM already composites
//! the desktop on whichever chip it likes and delivers textures onto *the device
//! given to the frame pool* — so there is no ownership rule to break:
//!
//! - nothing has to be restarted, because no launch-time decision is involved
//! - nothing is filed under an executable path, so renaming or copying Tain
//!   changes nothing
//! - no Windows setting is written at all
//!
//! It is the mode OBS tells people with gaming laptops to use, for this exact
//! fault.
//!
//! # Why it is not simply the only way
//!
//! Two costs, both real:
//!
//! - **The capture border.** WGC draws a yellow rectangle around what it
//!   captures. [`GraphicsCaptureSession::SetIsBorderRequired`] removes it, and
//!   only on Windows 11 21H2 and later — on Windows 10 the person being
//!   streamed watches a yellow box around their screen for as long as the
//!   session lasts.
//! - **It is DWM's schedule, not ours.** Duplication hands over a frame the
//!   moment the desktop changes; a frame pool delivers what the compositor
//!   produced, which is one more hop and one more buffer.
//!
//! So duplication stays first where it works, and this is what a machine falls
//! back to rather than what every machine uses. On the machines that fall back,
//! the alternative is not a border or an extra hop — it is no picture at all.
//!
//! # Polled, not evented
//!
//! The frame pool is created with `CreateFreeThreaded`, and frames are taken
//! with `TryGetNextFrame` rather than through the `FrameArrived` event. The
//! event needs a `DispatcherQueue` on the capturing thread, which means a
//! message pump on a thread whose whole job is to block on capture — and
//! [`super::capture_shared`] already gives this a thread of its own and a loop
//! that asks for frames. Polling fits that; an event loop would have to be
//! bolted beside it.
//!
//! A frame must be closed before the next one is asked for, or the pool runs
//! out of buffers and stops delivering. That is what [`Frame`] is for.

use anyhow::{Context, Result};
use bytes::Bytes;
use tracing::{debug, info, warn};
use windows::core::Interface;

use super::screen_capture::{PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame};

use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// How many buffers the compositor may queue for us.
///
/// Two. One being filled while one is read is the whole requirement, and every
/// extra buffer is a frame of latency the session pays for nothing — a remote
/// desktop would rather drop a frame than show an old one.
const BUFFERS: i32 = 2;

/// How long an unchanged screen goes before it is sent again.
///
/// Matches the duplication path's reasoning: a guest that joins a still desktop
/// has nothing to draw until something moves, so the picture is re-sent
/// occasionally to cover a lost frame or a late arrival.
const REFRESH_IDLE: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether this machine can capture this way at all.
///
/// False on Windows before 1903. Asked rather than assumed, because the answer
/// decides whether a fallback exists and saying "no adapter could capture"
/// while silently having skipped the thing that would have worked is the worst
/// of both.
pub fn is_available() -> bool {
    match GraphicsCaptureSession::IsSupported() {
        Ok(yes) => yes,
        Err(e) => {
            debug!("Windows.Graphics.Capture is not available here: {e}");
            false
        }
    }
}

/// A frame borrowed from the pool, returned when this is dropped.
///
/// The pool holds a fixed number of buffers and hands out one at a time; a
/// frame that is not closed is a buffer that never comes back, and after
/// [`BUFFERS`] of those the compositor has nowhere to put the next one and
/// delivery stops — with no error, because nothing failed. Tying the close to a
/// scope is the only way to be sure it happens on every path out, including the
/// ones that return an error partway through reading the texture.
struct Frame(windows::Graphics::Capture::Direct3D11CaptureFrame);

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

/// Capture of one display, through the compositor.
pub struct WgcCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// The WinRT device wrapping [`Self::device`], kept because recreating the
    /// pool after a resolution change needs it again.
    winrt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    /// Dropped last, and never before the pool: closing the session is what
    /// stops the compositor writing into buffers this still owns.
    session: GraphicsCaptureSession,

    /// BGRA readback target, rebuilt whenever the source size changes.
    staging: Option<ID3D11Texture2D>,
    /// The GPU colour conversion, when the encoder asked for NV12. The same
    /// video processor the duplication path uses — it rescales in the same pass
    /// it converts, which is why the emitted size is its business and not a
    /// separate resample here.
    converter: Option<super::capture_nv12::VideoScaler>,

    /// What the compositor is producing.
    width: u16,
    height: u16,
    /// What is emitted, once the scale and the guest's viewport are applied.
    out_width: u16,
    out_height: u16,
    scale: QualityScale,
    ceiling: Option<(u16, u16)>,
    granularity: (u16, u16),
    format: PixelFormat,

    /// Whether the pointer was showing the last time anybody asked.
    ///
    /// `None` until the first look. Kept so [`Self::take_cursor`] can report a
    /// change rather than the same answer on every frame.
    cursor_visible: Option<bool>,

    /// The last picture produced, **with the size it was produced at**.
    ///
    /// # Why the size travels with the bytes
    ///
    /// The idle refresh re-sends this when nothing has changed, labelled with
    /// whatever the emitted size is *now*. Those are not the same thing: the
    /// output size moves when the guest caps it, when the controller sheds
    /// pixels, and when the captured window is resized — and a window that has
    /// just been resized and then left alone produces no new frame to correct
    /// it with.
    ///
    /// So a stale buffer went out under a new, larger size and the encoder
    /// refused it — `Nv12 buffer too small: 1709568 < 3981312`, and the session
    /// ended. The duplication path avoids this by clearing its copy at every
    /// site that changes the size; this one did not, because remembering to do
    /// that at three call sites is exactly the kind of thing that gets
    /// forgotten at the fourth.
    ///
    /// Keeping the size beside the bytes makes them impossible to disagree:
    /// `idle_refresh` compares, and a mismatch means there is nothing worth
    /// re-sending rather than something that will be refused downstream.
    last_frame: Option<(u16, u16, Bytes)>,
    last_emitted: Option<std::time::Instant>,
    /// The display this is for, for the log line and for rebuilding.
    label: String,
}

// SAFETY: every WinRT and D3D object here is agile or free-threaded — the pool
// is created with `CreateFreeThreaded` precisely so it has no thread affinity,
// and the device is marked multithread-protected below. `capture_shared` builds
// this on its capture thread and uses it there; the bound exists because the
// trait requires it of every backend.
unsafe impl Send for WgcCapturer {}
unsafe impl Sync for WgcCapturer {}

/// What a compositor capture is pointed at.
///
/// The two are the same capture in every respect after the item exists — same
/// pool, same readback, same conversion — so they differ only in which interop
/// call produces the item.
enum Source {
    /// A display by device name, or the primary one.
    Monitor(Option<String>),
    /// One window by handle. See [`super::window_capture`].
    Window(u64),
}

/// [`Source`] once it has been resolved to something the interop factory takes.
enum Target {
    Monitor(windows::Win32::Graphics::Gdi::HMONITOR),
    Window(u64),
}

impl WgcCapturer {
    /// `want` is a device name as [`super::screen_capture::displays`] reports
    /// it; `None` means the primary display.
    pub fn new(want: Option<String>) -> Result<Self> {
        Self::of(Source::Monitor(want))
    }

    /// Capture one application window and nothing around it.
    ///
    /// Refuses a window that is not on screen. The compositor will not create
    /// an item for a minimised window, and a session that is *already* watching
    /// one keeps its capture alive across minimising — so the only case this
    /// has to refuse is starting on one, which the caller retries when the
    /// window comes back. See `window_capture::State`.
    pub fn for_window(id: u64) -> Result<Self> {
        use super::window_capture::State;
        match super::window_capture::state_of(id) {
            State::Showing => Self::of(Source::Window(id)),
            State::Minimised => {
                anyhow::bail!("that window is minimised, so there is nothing to capture yet")
            }
            State::Closed => anyhow::bail!("that window has closed"),
        }
    }

    fn of(source: Source) -> Result<Self> {
        anyhow::ensure!(
            is_available(),
            "this version of Windows has no compositor capture (it needs 1903 or later)"
        );

        // WinRT class activation needs the calling thread to be in an
        // apartment, and the capture thread is a bare `std::thread`. This is
        // the call meant for exactly that: it joins the process-wide MTA
        // without the thread having to own an apartment or pump messages. The
        // cookie is deliberately leaked — the MTA should outlive this capturer,
        // since another one may be built on another thread a moment later.
        // SAFETY: documented; the returned cookie is only for decrementing.
        let _ = unsafe { windows::Win32::System::Com::CoIncrementMTAUsage() };

        // Resolved before the device, because a target that does not exist is
        // worth failing on before a D3D11 device has been built for it.
        let (target, label) = match &source {
            Source::Monitor(want) => {
                let (monitor, label) = monitor_for(want.as_deref())?;
                (Target::Monitor(monitor), label)
            }
            Source::Window(id) => {
                let title = super::window_capture::title_of(*id)
                    .context("that window has gone")?;
                (Target::Window(*id), title)
            }
        };
        let (device, context) = create_device()?;

        // SAFETY: the device is live and implements IDXGIDevice.
        let winrt_device = unsafe {
            let dxgi: IDXGIDevice = device.cast().context("device is not an IDXGIDevice")?;
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
                .context("could not hand the D3D device to WinRT")?
        }
        .cast::<windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>()
        .context("the WinRT device is not an IDirect3DDevice")?;

        // The interop factory is how a Win32 handle becomes something the WinRT
        // API will capture. There is no other way in from desktop code.
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("no capture item factory — this Windows cannot capture a monitor")?;
        // SAFETY: a live handle, checked immediately above — a monitor from the
        // enumeration, or a window `for_window` has just found on screen.
        let item: GraphicsCaptureItem = unsafe {
            match target {
                Target::Monitor(monitor) => interop.CreateForMonitor(monitor),
                Target::Window(id) => interop.CreateForWindow(
                    windows::Win32::Foundation::HWND(id as isize as *mut std::ffi::c_void),
                ),
            }
        }
        .with_context(|| format!("{label} cannot be captured by the compositor"))?;

        let size = item.Size().context("the capture item has no size")?;
        let (width, height) = clamp_size(size);

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFERS,
            size,
        )
        .context("could not create the capture frame pool")?;

        let session = pool.CreateCaptureSession(&item).context("could not start a capture")?;

        // # The pointer is drawn into the frame here, unlike the other path
        //
        // Desktop Duplication reports the pointer separately — shape, position,
        // visibility — and the guest draws it locally, which is what makes it
        // move at the guest's frame rate rather than the stream's. WGC has no
        // equivalent: it can bake the pointer in or leave it out, and there is
        // no third answer. Left out, the guest would see no pointer at all, so
        // it goes in.
        let _ = session.SetIsCursorCaptureEnabled(true);

        // Windows 11 21H2 and later. On anything older this fails and the
        // yellow rectangle stays, which is a cost worth paying on a machine
        // whose alternative is not capturing at all — see the module note.
        if session.SetIsBorderRequired(false).is_err() {
            info!(
                "This Windows draws a capture border around {label}. \
                 It cannot be turned off before Windows 11 21H2."
            );
        }

        session.StartCapture().context("the compositor refused to start capturing")?;
        info!("Capturing {label} through the compositor at {width}x{height}");

        let mut capturer = Self {
            device,
            context,
            winrt_device,
            item,
            pool,
            session,
            staging: None,
            converter: None,
            width,
            height,
            out_width: width,
            out_height: height,
            scale: QualityScale::Native,
            ceiling: None,
            granularity: (2, 2),
            format: PixelFormat::Bgra,
            cursor_visible: None,
            last_frame: None,
            last_emitted: None,
            label,
        };
        capturer.recompute_output();
        Ok(capturer)
    }

    /// Work out what size to emit, from the scale, the ceiling and the encoder.
    ///
    /// Identical in intent to the duplication path's version: the controller
    /// sheds pixels under load, the guest caps what is worth sending, and the
    /// encoder says what multiples it will accept.
    fn recompute_output(&mut self) {
        let (mut w, mut h) = self.scale.apply((self.width, self.height));
        if let Some((cw, ch)) = self.ceiling {
            let (cw, ch) = (cw.max(1), ch.max(1));
            if w > cw || h > ch {
                let by = f64::from(cw) / f64::from(w);
                let by = by.min(f64::from(ch) / f64::from(h));
                w = ((f64::from(w) * by).round() as u16).max(1);
                h = ((f64::from(h) * by).round() as u16).max(1);
            }
        }
        let (w, h) = super::screen_capture::snap_to((w, h), self.granularity);
        if (w, h) == (self.out_width, self.out_height) {
            return;
        }
        self.out_width = w;
        self.out_height = h;
        // The converter is built for one output size. Dropping it here is what
        // makes the next frame rebuild it at the new one.
        self.converter = None;
    }

    /// Recompute the emitted size, and make sure a frame arrives at it.
    ///
    /// # Why asking is necessary
    ///
    /// The compositor delivers a frame when the *source* changes. Everything
    /// that reaches this — the guest capping the size, the controller shedding
    /// pixels, the encoder's granularity — changes what is emitted and not what
    /// is captured, so on a still source nothing would arrive to carry the new
    /// size. A desktop changes soon enough for that not to matter; a single
    /// window, resized and then left alone, does not. Without this the guest
    /// presses 1:1 and watches nothing happen.
    ///
    /// Recreating the pool at the size it already has is what provokes a
    /// delivery. Safe here because no frame is outstanding: these are the
    /// trait's setters, called from the capture loop between frames and never
    /// from inside `capture_next` while one is held — which is the thing
    /// `Recreate` must not race.
    ///
    /// Not used by [`Self::resize_to`], which has already recreated the pool at
    /// the new source size and would only be doing it twice.
    fn recompute_and_ask(&mut self) {
        let before = (self.out_width, self.out_height);
        self.recompute_output();
        if before == (self.out_width, self.out_height) {
            return;
        }
        if let Err(e) = self.pool.Recreate(
            &self.winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFERS,
            SizeInt32 { Width: i32::from(self.width), Height: i32::from(self.height) },
        ) {
            debug!("Could not ask {} for a fresh frame after a resize: {e}", self.label);
        }
    }

    /// Follow a resolution change on the captured display.
    ///
    /// The compositor keeps delivering the old size until the pool is told, and
    /// a frame that no longer matches is not an error anywhere — it is simply
    /// the wrong picture, cropped or stretched, for as long as nobody notices.
    fn resize_to(&mut self, size: SizeInt32) -> Result<()> {
        let (width, height) = clamp_size(size);
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        self.pool
            .Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, BUFFERS, size)
            .context("could not resize the capture frame pool")?;
        info!("{} changed to {width}x{height}", self.label);
        self.width = width;
        self.height = height;
        self.staging = None;
        self.converter = None;
        self.recompute_output();
        Ok(())
    }

    /// Turn one captured texture into the bytes a frame carries.
    fn read(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<u8>> {
        // # BGRA that has to shrink goes through the same processor
        //
        // The converter was built for the encoder's NV12, and the *resize* it
        // does on the way is the valuable part for everything else: a window
        // composite is assembled in BGRA, so its panels could not use it and
        // were resized on the CPU instead — measured at twenty to thirty
        // milliseconds a frame each, which is a whole frame budget for one
        // window and several for a grid of them.
        //
        // Converting BGRA to BGRA is a scaling blit as far as the driver is
        // concerned. Asked for only when the size actually changes: at 1:1
        // there is nothing to do and the readback is the whole job.
        let shrinking = (self.out_width, self.out_height) != (self.width, self.height);
        if self.format == PixelFormat::Nv12 || shrinking {
            let output = if self.format == PixelFormat::Nv12 {
                super::capture_nv12::Output::Nv12
            } else {
                super::capture_nv12::Output::Bgra
            };
            if self.converter.is_none() {
                match super::capture_nv12::VideoScaler::scaling(
                    &self.device,
                    &self.context,
                    self.width,
                    self.height,
                    self.out_width,
                    self.out_height,
                    output,
                ) {
                    Ok(built) => self.converter = Some(built),
                    // # A refusal is fatal for NV12 and survivable for BGRA
                    //
                    // The encoder was promised NV12 and cannot be told
                    // otherwise mid-stream, so that failure has to reach the
                    // caller. A resize has a CPU path that is correct and only
                    // slower, and taking it beats ending the session.
                    Err(e) if output == super::capture_nv12::Output::Nv12 => return Err(e),
                    Err(e) => {
                        info!("{}: resizing on the CPU instead: {e:#}", self.label);
                    }
                }
            }
            if let Some(converter) = self.converter.as_mut() {
                return converter.convert(texture);
            }
        }
        self.read_bgra(texture)
    }

    /// The plain path: copy to a staging texture and read it on the CPU.
    fn read_bgra(&mut self, texture: &ID3D11Texture2D) -> Result<Vec<u8>> {
        if self.staging.is_none() {
            self.staging = Some(staging_texture(&self.device, self.width, self.height)?);
        }
        let staging = self.staging.as_ref().expect("just built");

        // SAFETY: staging matches the source in format and size; it is unmapped
        // before this returns on every path.
        unsafe {
            self.context.CopyResource(staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not map the staging texture")?;
            let pixels = read_mapped(&mapped, self.width, self.height);
            self.context.Unmap(staging, 0);
            Ok(pixels)
        }
    }

    /// Re-send the last picture when nothing has changed for a while.
    fn idle_refresh(&mut self) -> Option<ScreenFrame> {
        let (width, height, last) = self.last_frame.clone()?;
        // Captured at a size this session has since stopped emitting. Sending
        // it would be sending the wrong number of bytes under the right
        // dimensions, which the encoder refuses and which ends the session —
        // see the field's own note. Nothing is the correct answer: the next
        // real frame arrives at the current size.
        if (width, height) != (self.out_width, self.out_height) {
            return None;
        }
        let due = self.last_emitted.is_none_or(|at| at.elapsed() >= REFRESH_IDLE);
        if !due {
            return None;
        }
        self.last_emitted = Some(std::time::Instant::now());
        Some(ScreenFrame::with_format(
            width,
            height,
            last,
            self.format,
            std::time::Duration::ZERO,
        ))
    }
}

impl ScreenCaptureBackend for WgcCapturer {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        // Null rather than an error is how the pool says "nothing yet", and it
        // is the ordinary answer on a still desktop.
        let Ok(raw) = self.pool.TryGetNextFrame() else {
            return Ok(self.idle_refresh());
        };
        let frame = Frame(raw);

        // # A resolution change is acted on with no frame outstanding
        //
        // `Recreate` reallocates the pool's buffers, and this frame is one of
        // them. Doing it here — with the frame still open, before reading the
        // texture — would leave the read working through a surface belonging to
        // a pool that had just been thrown away, which is not something that
        // fails cleanly.
        //
        // So the frame is handed back first and this one is skipped. Microsoft's
        // own sample does the same thing for the same reason. Losing a single
        // frame at the instant somebody changes their resolution costs nothing;
        // the next one arrives at the new size.
        let resized = frame
            .0
            .ContentSize()
            .ok()
            .filter(|size| clamp_size(*size) != (self.width, self.height));
        if let Some(size) = resized {
            drop(frame);
            if let Err(e) = self.resize_to(size) {
                warn!("Could not follow a resolution change on {}: {e:#}", self.label);
            }
            return Ok(None);
        }

        let started = std::time::Instant::now();
        let surface = frame.0.Surface().context("a captured frame carried no surface")?;
        let access: IDirect3DDxgiInterfaceAccess =
            surface.cast().context("the frame surface is not a DXGI surface")?;
        // SAFETY: the surface is live for as long as `frame` is, which outlives
        // the read below.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.context("the frame surface is not a texture")?;

        let pixels = self.read(&texture)?;
        let cost = started.elapsed();

        // # Only when the GPU did not already do it
        //
        // The video processor now produces the emitted size for BGRA as well as
        // for NV12, so this is the fallback for a driver that refused — see
        // `read`. Left ungated it ran *after* the GPU scale, treating an
        // already-shrunk buffer as if it were still full size, and read off the
        // end of it. Caught by the composite panicking one index past its own
        // frame.
        let (pixels, cost) = if self.converter.is_none()
            && self.format == PixelFormat::Bgra
            && (self.out_width, self.out_height) != (self.width, self.height)
        {
            let scaling = std::time::Instant::now();
            let full = ScreenFrame::new(self.width, self.height, Bytes::from(pixels), cost);
            let scaled =
                super::screen_capture::box_downscale(&full, self.out_width, self.out_height);
            (scaled, cost + scaling.elapsed())
        } else {
            (pixels, cost)
        };

        let data = Bytes::from(pixels);
        self.last_frame = Some((self.out_width, self.out_height, data.clone()));
        self.last_emitted = Some(std::time::Instant::now());
        Ok(Some(ScreenFrame::with_format(
            self.out_width,
            self.out_height,
            data,
            self.format,
            cost,
        )))
    }

    /// Whether the pointer is showing, and nothing else.
    ///
    /// # No shape, deliberately
    ///
    /// The pointer is already drawn into the frame — see the note in
    /// [`WgcCapturer::new`], where that is decided and why there is no choice.
    /// Sending an image as well would put a second pointer on top of the first.
    /// The guest only draws when it has both a position *and* a shape, so an
    /// update with no shape carries the fact without the ghost.
    ///
    /// # Why the fact is worth carrying on its own
    ///
    /// Because the guest decides whether to send the pointer as a movement or
    /// as a place by exactly this, and until now this path answered nothing at
    /// all. A host captured through the compositor therefore looked, from the
    /// guest, like one whose cursor was showing — so positions were sent, and a
    /// position runs out at the edge of the window. In a game that is a camera
    /// that stops turning while the mouse keeps moving. See
    /// `GuestSession::wants_relative_pointer`.
    ///
    /// Reported only when it changes. `x` and `y` are the pointer's place on
    /// the desktop rather than within whatever is being captured, which is of
    /// no use to anybody here and is why nothing reads them: a window share's
    /// frame does not start at the desktop's origin. Only `visible` is meant.
    fn take_cursor(&mut self) -> Option<super::cursor::CursorUpdate> {
        let (showing, at) = pointer_showing()?;
        if self.cursor_visible == Some(showing) {
            return None;
        }
        self.cursor_visible = Some(showing);
        Some(super::cursor::CursorUpdate {
            position: super::cursor::CursorPosition {
                x: at.0,
                y: at.1,
                desktop_width: self.width,
                desktop_height: self.height,
                visible: showing,
            },
            shape: None,
        })
    }

    fn frame_size(&self) -> (u16, u16) {
        (self.out_width, self.out_height)
    }

    fn set_scale(&mut self, scale: QualityScale) -> bool {
        self.scale = scale;
        self.recompute_and_ask();
        true
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        self.ceiling = ceiling;
        self.recompute_and_ask();
        true
    }

    fn set_size_granularity(&mut self, granularity: (u16, u16)) -> bool {
        self.granularity = (granularity.0.max(1), granularity.1.max(1));
        self.recompute_and_ask();
        true
    }

    fn request_format(&mut self, format: PixelFormat) -> bool {
        if format != PixelFormat::Nv12 {
            return false;
        }
        // Built lazily on the next frame, where the source texture is in hand.
        // Answering yes before it exists is safe because the only thing that
        // can refuse is the video processor, and a machine whose driver has one
        // for the duplication path has the same one here.
        self.format = PixelFormat::Nv12;
        self.converter = None;
        self.recompute_and_ask();
        true
    }

    fn current_format(&self) -> PixelFormat {
        self.format
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        // Order matters: the session is what has the compositor writing into
        // the pool's buffers, so it stops first.
        let _ = self.session.Close();
        let _ = self.pool.Close();
        let _ = &self.item;
    }
}

/// Whether the pointer is being drawn anywhere, and where it is.
///
/// # Why this is asked of Windows rather than of the capture
///
/// Because the capture has no answer. Desktop Duplication reports the pointer
/// as a thing apart — shape, place, whether it is showing — and the compositor
/// does not: it either paints the pointer into the frame or leaves it out, and
/// says nothing either way. `GetCursorInfo` is the same fact from the other
/// direction, it costs a syscall, and it is true regardless of which backend
/// is running. See [`WgcCapturer::take_cursor`] for what it is used for.
///
/// `None` when the call fails, which leaves the previous answer standing
/// rather than reporting a change that was never observed.
///
/// Shared with [`super::capture_wall`], which bakes the pointer into its
/// panels for exactly the same reason and so needs exactly the same answer.
pub(crate) fn pointer_showing() -> Option<(bool, (i32, i32))> {
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

    let mut info =
        CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    // SAFETY: `cbSize` is filled in as the call documents, and `info` outlives
    // it. Nothing is retained.
    unsafe { GetCursorInfo(&mut info) }.ok()?;
    Some((
        info.flags.0 & CURSOR_SHOWING.0 != 0,
        (info.ptScreenPos.x, info.ptScreenPos.y),
    ))
}

/// A size the rest of the pipeline can carry, and never zero.
fn clamp_size(size: SizeInt32) -> (u16, u16) {
    (
        size.Width.clamp(1, u16::MAX as i32) as u16,
        size.Height.clamp(1, u16::MAX as i32) as u16,
    )
}

/// The monitor handle for a display name, and something to call it.
///
/// Matched on `szDevice` — `\\.\DISPLAY1` — which is the same string
/// [`super::screen_capture::displays`] reports and the same one the duplication
/// path matches on, so a display asked for by name reaches the same screen
/// whichever backend answers.
fn monitor_for(
    want: Option<&str>,
) -> Result<(windows::Win32::Graphics::Gdi::HMONITOR, String)> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };

    /// `MONITORINFOF_PRIMARY`, which the crate does not name.
    const PRIMARY: u32 = 0x0000_0001;

    struct Search {
        want: Option<String>,
        found: Option<(HMONITOR, String)>,
        seen: Vec<String>,
    }

    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        state: LPARAM,
    ) -> BOOL {
        // SAFETY: the pointer is the `Search` handed to EnumDisplayMonitors
        // below, which outlives the enumeration.
        let search = unsafe { &mut *(state.0 as *mut Search) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `info` is sized as the API requires and outlives the call.
        if !unsafe {
            GetMonitorInfoW(monitor, std::ptr::addr_of_mut!(info) as *mut MONITORINFO)
        }
        .as_bool()
        {
            return TRUE;
        }

        let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..end]);
        search.seen.push(name.clone());

        let matches = match search.want.as_deref() {
            Some(wanted) => name == wanted,
            // The primary, which is the one Windows flags as such rather than
            // the one at the origin — those agree, and the flag says so
            // directly.
            None => info.monitorInfo.dwFlags & PRIMARY != 0,
        };
        if matches && search.found.is_none() {
            search.found = Some((monitor, name));
        }
        TRUE
    }

    let mut search =
        Search { want: want.map(str::to_string), found: None, seen: Vec::new() };
    // SAFETY: the callback only touches `search`, which outlives this call.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(std::ptr::addr_of_mut!(search) as isize),
        );
    }

    search.found.with_context(|| {
        format!(
            "{} is not one of this machine's monitors (it has {})",
            want.unwrap_or("the primary display"),
            if search.seen.is_empty() {
                "none".to_string()
            } else {
                search.seen.join(", ")
            }
        )
    })
}

/// A D3D11 device for the frame pool to deliver onto.
///
/// The default adapter, deliberately: unlike duplication, this does not have to
/// be the chip drawing the desktop — that freedom is the entire reason this
/// module exists. Whichever one this process was given is one the compositor
/// can deliver to.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut last = None;

    // VIDEO_SUPPORT is what makes the NV12 conversion available. Asked for and
    // given up rather than insisted on, as on the duplication path: a driver
    // that refuses it still captures, and losing the conversion costs a CPU
    // pass per frame where losing the device costs the session.
    for flags in [
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ] {
        // SAFETY: all out-params are valid; a failed call leaves them None.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => break,
            Err(e) => last = Some(e),
        }
    }

    let device = device
        .ok_or_else(|| anyhow::anyhow!("could not create a D3D11 device: {last:?}"))?;
    let context = context.context("D3D11 device came back without a context")?;

    // The pool is free-threaded and the compositor writes into it from its own
    // thread. Without this the driver may assume single-threaded access, and
    // what that produces is a corrupt frame or a device removal rather than an
    // error anyone can trace back here.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: the device is live; this only sets a flag on it.
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Ok((device, context))
}

/// A CPU-readable copy target matching the captured surface.
fn staging_texture(device: &ID3D11Device, width: u16, height: u16) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: u32::from(width),
        Height: u32::from(height),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: the description is fully initialised and the out-param is valid.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("could not create the staging texture")?;
    texture.context("CreateTexture2D returned nothing")
}

/// Copy a mapped BGRA surface into a tightly packed buffer.
///
/// `RowPitch` is almost never `width * 4` — the driver pads rows for alignment,
/// so copying the whole mapping in one go produces a skewed image.
///
/// # Safety
/// `mapped.pData` must point to at least `height * mapped.RowPitch` readable bytes.
unsafe fn read_mapped(mapped: &D3D11_MAPPED_SUBRESOURCE, width: u16, height: u16) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let pitch = mapped.RowPitch as usize;
    let mut out = vec![0u8; row_bytes * height as usize];

    for y in 0..height as usize {
        let src = unsafe { (mapped.pData as *const u8).add(y * pitch) };
        let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
        // SAFETY: the row lies inside the mapped region and dst is exactly row_bytes.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), row_bytes) };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one window costs per frame, on each of the two paths.
    ///
    /// # Why this exists
    ///
    /// A single shared window was reported streaming at sixteen frames a second
    /// with both machines idle, and the adaptive controller settles exactly
    /// there when capture is spending forty to sixty milliseconds a frame. The
    /// two candidates are the colour conversion and the resize, and they run in
    /// different places depending on what the encoder negotiated — so the
    /// number that matters is per path, not overall.
    ///
    /// Frames are provoked rather than waited for: a still window delivers
    /// nothing, and `set_size_ceiling` recreates the pool, which makes the
    /// compositor hand one over. That costs a pool recreation per sample, which
    /// is why the *conversion* is timed from the frame's own reported cost
    /// rather than from the wall clock around it.
    #[test]
    #[ignore = "captures a real window — run explicitly"]
    fn report_what_one_window_costs_on_each_path() {
        let Some(window) = crate::window_capture::shareable().into_iter().max_by_key(|w| {
            u32::from(w.width) * u32::from(w.height)
        }) else {
            println!("nothing open to capture");
            return;
        };
        println!("{} — {}x{}", window.app, window.width, window.height);

        for nv12 in [false, true] {
            let Ok(mut capturer) = WgcCapturer::for_window(window.id) else {
                println!("  could not capture it");
                return;
            };
            let asked = nv12 && capturer.request_format(PixelFormat::Nv12);
            if nv12 && !asked {
                println!("  NV12: refused");
                continue;
            }

            let mut costs = Vec::new();
            for round in 0..24 {
                // A ceiling that alternates by two pixels, so each round is a
                // real reconfiguration and a frame actually arrives.
                capturer.set_size_ceiling(Some((1200 + (round % 2) * 2, 750)));
                for _ in 0..50 {
                    if let Ok(Some(frame)) = capturer.capture_next() {
                        costs.push(frame.cost.as_secs_f64() * 1000.0);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            costs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let middle = costs.get(costs.len() / 2).copied().unwrap_or(0.0);
            let (out_w, out_h) = capturer.frame_size();
            println!(
                "  {} -> {out_w}x{out_h}: {} frames, median {middle:.1} ms",
                if asked { "NV12" } else { "BGRA" },
                costs.len(),
            );
        }
    }

    /// What a window costs to capture, at full size and shrunk.
    ///
    /// Reported rather than asserted. It exists because a composite was
    /// measured running at six to ten frames a second with both machines' GPUs
    /// idle, and the question that answers is where the time goes: the readback,
    /// or the resample that follows it.
    #[test]
    #[ignore = "captures a real window — run explicitly"]
    fn report_what_a_window_costs_per_frame() {
        let Some(window) = crate::window_capture::shareable().into_iter().max_by_key(|w| {
            u32::from(w.width) * u32::from(w.height)
        }) else {
            println!("nothing open to capture");
            return;
        };
        println!("{} — {}x{}", window.app, window.width, window.height);

        for ceiling in [None, Some((960u16, 600u16))] {
            let Ok(mut capturer) = WgcCapturer::for_window(window.id) else {
                println!("  could not capture it");
                return;
            };
            capturer.set_size_ceiling(ceiling);
            let (out_w, out_h) = capturer.frame_size();

            // The first frames carry setup; what matters is the steady state.
            let mut costs = Vec::new();
            for _ in 0..40 {
                if let Ok(Some(frame)) = capturer.capture_next() {
                    costs.push(frame.cost.as_secs_f64() * 1000.0);
                }
            }
            costs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let middle = costs.get(costs.len() / 2).copied().unwrap_or(0.0);
            println!(
                "  ceiling {ceiling:?} -> {out_w}x{out_h}: {} frames, median {middle:.1} ms",
                costs.len()
            );
        }
    }

    /// Where the time goes when a composite is built.
    ///
    /// Reported rather than asserted — it is a property of the graphics driver.
    /// It exists because "the composite takes a while to appear" needed
    /// attributing to something, and the two candidates are the graphics device
    /// and the capture session, which want completely different fixes.
    #[test]
    #[ignore = "creates real graphics devices — run explicitly"]
    fn report_what_a_capture_costs_to_set_up() {
        for round in 0..4 {
            let started = std::time::Instant::now();
            let made = create_device();
            println!(
                "  device {round}: {:.1} ms ({})",
                started.elapsed().as_secs_f64() * 1000.0,
                if made.is_ok() { "ok" } else { "failed" }
            );
        }
    }

    /// A frame is only ever re-sent at the size it was captured at.
    ///
    /// # The crash this is for
    ///
    /// `Nv12 buffer too small: 1709568 < 3981312`, and the session ended. The
    /// idle refresh re-sent the last picture labelled with whatever the emitted
    /// size had become — and the emitted size moves when the guest caps it,
    /// when the controller sheds pixels, or when the captured window is
    /// resized. Resize a window, then press 1:1 on the guest: the size changed,
    /// nothing moved, so the stale buffer went out under the new dimensions and
    /// the encoder refused it.
    ///
    /// The size now travels with the bytes, so the two cannot disagree. This
    /// checks the decision that rests on it, without needing a compositor: the
    /// bytes are re-sent when the size matches and withheld when it does not.
    #[test]
    fn a_stale_picture_is_not_re_sent_under_a_new_size() {
        /// What `idle_refresh` decides, with the timing taken out.
        fn would_resend(stored: (u16, u16), emitting: (u16, u16)) -> bool {
            stored == emitting
        }

        assert!(would_resend((1920, 1080), (1920, 1080)), "an unchanged size is re-sendable");

        // Every way the emitted size moves without a new frame arriving.
        for grown in [(2560, 1600), (1920, 1081), (1921, 1080)] {
            assert!(
                !would_resend((1920, 1080), grown),
                "a 1920x1080 buffer was offered as {grown:?} — this is the crash"
            );
        }
        // Shrinking is just as wrong: the encoder checks the length against the
        // dimensions, and too many bytes is a different picture, not a safe one.
        assert!(!would_resend((1920, 1080), (1280, 720)));
    }

    /// A size from the compositor must never reach the pipeline as zero.
    #[test]
    fn a_size_is_clamped_into_something_capturable() {
        assert_eq!(clamp_size(SizeInt32 { Width: 1920, Height: 1080 }), (1920, 1080));
        // A monitor being reconfigured reports these, and a zero-sized frame is
        // a division by zero several layers up.
        assert_eq!(clamp_size(SizeInt32 { Width: 0, Height: 0 }), (1, 1));
        assert_eq!(clamp_size(SizeInt32 { Width: -1, Height: 1080 }), (1, 1080));
        // And nothing here can carry a size the protocol cannot.
        assert_eq!(clamp_size(SizeInt32 { Width: 100_000, Height: 1080 }), (u16::MAX, 1080));
    }

    /// Whether this machine can capture through the compositor at all.
    ///
    /// A diagnostic rather than an assertion: the answer is a property of the
    /// Windows version, and both answers are correct on some machine.
    #[test]
    fn what_this_machine_says_about_compositor_capture() {
        println!("Windows.Graphics.Capture supported: {}", is_available());
    }

    /// Capture one real frame from this machine's primary display.
    ///
    /// # Why this is worth running on a real machine
    ///
    /// Every interesting thing here is the compositor's answer rather than this
    /// code's: whether a monitor can be turned into a capture item, whether the
    /// pool delivers without a dispatcher queue, whether the surface hands back
    /// a texture, and whether the frame has anything in it. None of that can be
    /// asserted without a desktop, and all of it decides whether this is a
    /// fallback worth having.
    ///
    /// Run with
    /// `cargo test -p tain-core --lib -- --ignored --nocapture compositor_capture_produces`.
    #[test]
    #[ignore = "captures this machine's screen — run explicitly"]
    fn compositor_capture_produces_a_frame_of_this_desktop() {
        if !is_available() {
            println!("No compositor capture on this Windows — nothing to test.");
            return;
        }
        let mut capturer = WgcCapturer::new(None).expect("could not start compositor capture");
        let (width, height) = capturer.frame_size();
        println!("Capturing {width}x{height}");

        // The first frame is not always ready the instant capture starts, so
        // this asks for a moment rather than once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let frame = loop {
            match capturer.capture_next().expect("capture failed") {
                Some(frame) => break frame,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                None => panic!("no frame arrived within 5s"),
            }
        };

        assert_eq!((frame.width, frame.height), (width, height));
        assert_eq!(
            frame.data.len(),
            frame.width as usize * frame.height as usize * 4,
            "a BGRA frame is four bytes a pixel"
        );
        // A desktop that is entirely one colour is what a frame of nothing
        // looks like, and it is the failure this test exists to catch.
        let distinct: std::collections::BTreeSet<&[u8]> =
            frame.data.chunks_exact(4).step_by(997).take(500).collect();
        println!("  {} bytes, {} distinct sampled pixels", frame.data.len(), distinct.len());
        assert!(distinct.len() > 1, "every sampled pixel was identical — the frame is blank");
    }

    /// One window, and only that window.
    ///
    /// # What this proves that the monitor test does not
    ///
    /// Capturing a display and capturing a window are two different interop
    /// calls into the same machinery, and only one of them can fail on a
    /// machine where the other works — `CreateForWindow` is refused for a
    /// window that is minimised, on another desktop, or protected from capture,
    /// none of which has an equivalent for a monitor. And the frame that comes
    /// back must be the *window's* size rather than the screen's, which is the
    /// difference between sharing a window and cropping one.
    ///
    /// Run with
    /// `cargo test -p tain-core --lib -- --ignored --nocapture a_single_window_captures`.
    #[test]
    #[ignore = "captures a window on this machine — run explicitly"]
    fn a_single_window_captures_at_its_own_size() {
        if !is_available() {
            println!("No compositor capture on this Windows — nothing to test.");
            return;
        }
        let offered = super::super::window_capture::shareable();
        let Some(window) = offered.first() else {
            println!("Nothing shareable is open — open a window and run this again.");
            return;
        };
        println!("Capturing {} — {} ({}x{})", window.app, window.title, window.width, window.height);

        let mut capturer =
            WgcCapturer::for_window(window.id).expect("could not capture that window");
        let (width, height) = capturer.frame_size();

        // The window's own size, not the screen's. Compared loosely because the
        // two disagree by the invisible resize border a window carries and the
        // capture does not — a few pixels, never a screen's worth.
        let screen = super::super::screen_capture::primary_display_size();
        println!("  captured {width}x{height}, this screen is {screen:?}");
        assert!(
            i32::from(width) - i32::from(window.width) < 40
                && i32::from(window.width) - i32::from(width) < 40,
            "{width} is not the window's own width of {}",
            window.width
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let frame = loop {
            match capturer.capture_next().expect("capture failed") {
                Some(frame) => break frame,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                None => panic!("no frame arrived within 5s"),
            }
        };
        assert_eq!(
            frame.data.len(),
            frame.width as usize * frame.height as usize * 4,
            "a BGRA frame is four bytes a pixel"
        );
        // Spread over the whole picture, not the first few rows: a window's top
        // edge is its title bar, and five hundred samples of one flat colour
        // would pass this while the rest of the frame was empty.
        let pixels = frame.data.len() / 4;
        let stride = (pixels / 500).max(1);
        let distinct: std::collections::BTreeSet<&[u8]> =
            frame.data.chunks_exact(4).step_by(stride).collect();
        println!("  {} bytes, {} distinct sampled pixels", frame.data.len(), distinct.len());
        assert!(distinct.len() > 1, "every sampled pixel was identical — the frame is blank");
    }

    /// The path a real session actually takes: NV12, converted on the GPU.
    ///
    /// # Why this is separate from the frame above
    ///
    /// The encoder asks for NV12, and getting it is what keeps a full-resolution
    /// colour conversion off the CPU — at 2560x1600 that is fourteen megabytes a
    /// frame. So BGRA capturing correctly proves the plumbing and *not* the
    /// thing that runs: a video processor that refuses this size, or produces
    /// the wrong number of bytes, is a fault that only appears here.
    ///
    /// Run with
    /// `cargo test -p tain-core --lib -- --ignored --nocapture compositor_capture_converts`.
    #[test]
    #[ignore = "captures this machine's screen — run explicitly"]
    fn compositor_capture_converts_to_nv12_on_the_gpu() {
        if !is_available() {
            println!("No compositor capture on this Windows — nothing to test.");
            return;
        }
        let mut capturer = WgcCapturer::new(None).expect("could not start compositor capture");
        assert!(
            capturer.request_format(PixelFormat::Nv12),
            "the compositor backend refused the format the encoder asks for"
        );
        assert_eq!(capturer.current_format(), PixelFormat::Nv12);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let frame = loop {
            match capturer.capture_next().expect("capture failed") {
                Some(frame) => break frame,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                None => panic!("no frame arrived within 5s"),
            }
        };

        assert_eq!(frame.format, PixelFormat::Nv12, "the frame is not what was asked for");
        // NV12 is one byte of luma per pixel plus a half-resolution chroma
        // plane: twelve bits a pixel, which is the whole reason for asking.
        let expected = frame.width as usize * frame.height as usize * 3 / 2;
        assert_eq!(
            frame.data.len(),
            expected,
            "{}x{} of NV12 should be {expected} bytes",
            frame.width,
            frame.height
        );
        println!("  NV12 {}x{}, {} bytes", frame.width, frame.height, frame.data.len());
    }
}
