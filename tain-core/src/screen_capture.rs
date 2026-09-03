//! Cross-platform screen capture abstraction.
//!
//! Capture resolution is **derived from the host's display**, never picked by
//! hand: a remote desktop shows the host's screen, so the native size is the
//! only correct starting point. What the user chooses is a [`QualityScale`] —
//! how much of that resolution to spend bandwidth on.

use anyhow::Result;
use bytes::Bytes;
use std::sync::Mutex as StdMutex;
use tracing::*;

/// Assumed display size when the platform cannot be queried.
const FALLBACK_DISPLAY: (u16, u16) = (1920, 1080);

/// Fraction of the host's native resolution to stream.
///
/// Downscaling is the main bandwidth lever while the pipeline is uncompressed —
/// halving each axis quarters the byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityScale {
    /// Full host resolution. Sharpest, most bandwidth.
    #[default]
    Native,
    /// 75% of each axis.
    High,
    /// 50% of each axis.
    Medium,
    /// One third of each axis. Legible text is unlikely.
    Low,
}

impl QualityScale {
    pub const ALL: [Self; 4] = [Self::Native, Self::High, Self::Medium, Self::Low];

    pub fn factor(self) -> f32 {
        match self {
            Self::Native => 1.0,
            Self::High => 0.75,
            Self::Medium => 0.5,
            Self::Low => 1.0 / 3.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "Native",
            Self::High => "High (75%)",
            Self::Medium => "Medium (50%)",
            Self::Low => "Low (33%)",
        }
    }

    /// Apply to a native size, keeping both axes even and at least 2px.
    ///
    /// Odd dimensions break chroma subsampling in every real video codec, so
    /// rounding down to even here avoids a whole class of encoder bugs later.
    pub fn apply(self, native: (u16, u16)) -> (u16, u16) {
        let scale = |v: u16| {
            let scaled = (v as f32 * self.factor()).round() as u32;
            (scaled.clamp(2, u16::MAX as u32) as u16) & !1
        };
        (scale(native.0).max(2), scale(native.1).max(2))
    }
}

/// The host's primary display size in pixels.
///
/// Multi-monitor spanning is a later feature; this deliberately reports the
/// primary display rather than the virtual desktop, so a second monitor does not
/// silently double what gets streamed.
///
/// **This must report physical pixels, not DPI-virtualised ones.** The obvious
/// call, `GetSystemMetrics(SM_CXSCREEN)`, does not: a process that has not
/// declared DPI awareness is told a 2560×1600 display at 175% scaling is
/// 1463×914. Believing that would mean "native resolution" streamed 31% of the
/// pixels the user actually has and looked soft for no visible reason.
/// `EnumDisplaySettings` reports the display mode, which is never virtualised,
/// so the answer does not depend on when — or whether — anything declared DPI
/// awareness. Capture itself is unaffected either way, because DXGI reports its
/// own size and [`create_capturer`] scales against that; this figure drives the
/// bandwidth estimate in the settings UI and the measurement harness.
pub fn primary_display_size() -> (u16, u16) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Graphics::Gdi::{
            EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
        };

        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: a null device name means the primary display; `mode` is sized
        // as the API requires and outlives the call.
        let ok = unsafe {
            EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode).as_bool()
        };
        if ok && mode.dmPelsWidth > 0 && mode.dmPelsHeight > 0 {
            return (
                mode.dmPelsWidth.min(u16::MAX as u32) as u16,
                mode.dmPelsHeight.min(u16::MAX as u32) as u16,
            );
        }

        // SAFETY: GetSystemMetrics takes an index and returns an int; no pointers.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if w > 0 && h > 0 {
            debug!("EnumDisplaySettings failed; using the metrics figure {w}x{h}");
            return (w.min(u16::MAX as i32) as u16, h.min(u16::MAX as i32) as u16);
        }
        warn!("Could not determine the display size; falling back to {FALLBACK_DISPLAY:?}");
    }

    #[cfg(not(target_os = "windows"))]
    debug!("No display query on this platform; assuming {FALLBACK_DISPLAY:?}");

    FALLBACK_DISPLAY
}

/// A display's real mode: size in physical pixels, and where it sits.
///
/// Split out because it is the one part of [`displays`] that must not come from
/// DXGI — see the note at the call site.
#[cfg(target_os = "windows")]
fn display_mode(device_name: &[u16; 32]) -> Option<(u16, u16, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: the name is a null-terminated UTF-16 array from `DXGI_OUTPUT_DESC`,
    // which is what this call wants; `mode` is sized as the API requires and
    // outlives the call.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
    };
    if !ok || mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 {
        return None;
    }

    // SAFETY: `dmPosition` is valid whenever `DM_POSITION` is set, which
    // `ENUM_CURRENT_SETTINGS` always sets for an attached display. Reading it
    // otherwise yields zeroes, which is the primary display's own position and
    // therefore harmless.
    let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
    Some((
        mode.dmPelsWidth.min(u16::MAX as u32) as u16,
        mode.dmPelsHeight.min(u16::MAX as u32) as u16,
        position.x,
        position.y,
    ))
}

/// Every display attached to this machine.
///
/// # Why DXGI and not `EnumDisplayMonitors`
///
/// GDI would give the geometry with less ceremony, and would then need DXGI
/// anyway to answer the one question that cannot be asked any other way:
/// whether the desktop on a given display is HDR. Asking one API for all of it
/// keeps the answers consistent — a display that DXGI cannot describe is a
/// display capture cannot use either, and it is better to leave it out of the
/// list than to offer a guest something that will fail when it is selected.
///
/// Ordered as the driver enumerates them, which is what [`DisplayInfo::id`]
/// indexes. Empty on a machine that cannot be asked, which callers must read as
/// "unknown", never as "no displays" — see [`super::protocol::SessionStart`].
pub fn displays() -> Vec<super::protocol::DisplayInfo> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

        // SAFETY: enumeration only. Every interface is created here, read, and
        // dropped before returning; nothing outlives the call.
        let found = unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
                warn!("No DXGI factory; cannot enumerate displays");
                return Vec::new();
            };

            let mut found = Vec::new();
            let mut id = 0u32;
            for adapter_index in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(adapter_index) else { break };
                for output_index in 0.. {
                    let Ok(output) = adapter.EnumOutputs(output_index) else { break };
                    let Ok(desc) = output.GetDesc() else { continue };
                    // A display that is not on the desktop cannot be captured
                    // and cannot be pointed at, so it is not one to offer.
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }

                    let end = desc
                        .DeviceName
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.DeviceName.len());
                    let name = String::from_utf16_lossy(&desc.DeviceName[..end]);
                    let hdr = super::capture_hdr::probe(&output)
                        .is_some_and(super::capture_hdr::DisplayColour::is_hdr);

                    // # Not `desc.DesktopCoordinates`
                    //
                    // Those are DPI-virtualised in a process that has not
                    // declared awareness — measured: a 2560x1600 display at
                    // 175% scaling reported as 1463x914, the same trap
                    // `primary_display_size` documents and avoids for the same
                    // reason. The application declares awareness and the agent
                    // does too, so this would have been right where it runs and
                    // wrong everywhere else, including under `cargo test`.
                    //
                    // The display *mode* is never virtualised, so asking for it
                    // by device name gives an answer that does not depend on
                    // what the calling process happened to declare.
                    let Some((width, height, x, y)) = display_mode(&desc.DeviceName) else {
                        warn!("{name} would not report its display mode; leaving it out");
                        continue;
                    };

                    found.push(super::protocol::DisplayInfo {
                        id,
                        name,
                        width,
                        height,
                        x,
                        y,
                        // Windows puts the primary display at the origin of the
                        // virtual desktop by definition, so this needs no
                        // separate query.
                        primary: x == 0 && y == 0,
                        hdr,
                    });
                    id += 1;
                }
            }
            found
        };

        if found.is_empty() {
            warn!("DXGI reported no display attached to the desktop");
        }
        found
    }

    #[cfg(not(target_os = "windows"))]
    {
        debug!("No display enumeration on this platform");
        Vec::new()
    }
}

/// Configuration for screen capture.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    /// Which display to capture, as a device name — `\\.\DISPLAY2` and the
    /// like, exactly as [`displays`] reports it.
    ///
    /// `None` is the primary display, which is what a session that has not
    /// asked for anything else gets. A name rather than the wire's numeric id
    /// because the id is an enumeration index and renumbers when a monitor is
    /// plugged in; the name does not.
    pub display: Option<String>,
    /// Target frames per second.
    pub fps: u32,
    /// How much of the host's native resolution to stream.
    pub quality: QualityScale,
    /// Whether to include the mouse cursor in captured frames.
    pub show_cursor: bool,
    /// Force a synthetic display of this size instead of capturing the real one.
    ///
    /// Setting this selects the test pattern backend, which is what makes tests
    /// deterministic and cheap. Production leaves it `None` so capture always
    /// follows the actual screen.
    pub native_override: Option<(u16, u16)>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            display: None,
            fps: 30,
            quality: QualityScale::default(),
            show_cursor: true,
            native_override: None,
        }
    }
}

impl CaptureConfig {
    /// The host's native display size.
    pub fn native_size(&self) -> (u16, u16) {
        self.native_override.unwrap_or_else(primary_display_size)
    }

    /// Frame size actually streamed, after applying the quality scale.
    pub fn dimensions(&self) -> (u16, u16) {
        self.quality.apply(self.native_size())
    }

    /// Bytes per raw BGRA frame at the streamed resolution.
    pub fn frame_bytes(&self) -> usize {
        let (w, h) = self.dimensions();
        w as usize * h as usize * 4
    }

    /// Uncompressed bandwidth at the configured frame rate, in MB/s.
    pub fn raw_mb_per_sec(&self) -> f32 {
        (self.frame_bytes() as f32 * self.fps as f32) / (1024.0 * 1024.0)
    }
}

/// How the bytes in a [`ScreenFrame`] are laid out.
///
/// There are only two, and the second exists for one reason: the hardware
/// encoder wants NV12, and every path that hands it BGRA pays twice — once to
/// read four bytes per pixel back off the GPU, and again to convert them on the
/// CPU. A capture backend that can do the conversion on the GPU removes both,
/// so it needs a way to say what it produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Four bytes per pixel, B-G-R-A. What every backend produces by default
    /// and what every consumer understands.
    Bgra,
    /// Twelve bits per pixel: a full-resolution luma plane followed by an
    /// interleaved half-resolution chroma plane. BT.601, limited range — the
    /// same convention as the CPU converter, because a mismatch shifts the
    /// colour of the whole picture depending on which path happened to run.
    Nv12,
}

impl PixelFormat {
    /// Bytes one frame of this size occupies.
    pub fn frame_bytes(&self, width: u16, height: u16) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            Self::Bgra => w * h * 4,
            Self::Nv12 => w * h + (w.div_ceil(2) * 2) * h.div_ceil(2),
        }
    }
}

/// A captured screen frame.
#[derive(Debug, Clone)]
pub struct ScreenFrame {
    pub width: u16,
    pub height: u16,
    /// Pixel data, laid out according to [`Self::format`].
    pub data: Bytes,
    /// What the bytes mean. Consumers must check this rather than assuming
    /// BGRA — a frame read as the wrong format is a green-and-magenta mess
    /// that still passes every length check.
    pub format: PixelFormat,
    /// Time the backend spent *producing* this frame — the GPU copy, the
    /// readback, any rescale — and **not** the time it spent waiting for the
    /// desktop to change.
    ///
    /// The distinction is the whole point. Desktop Duplication blocks until
    /// something moves, so timing the call from outside charges an idle screen
    /// the full wait and reports it as slow capture: a machine doing nothing,
    /// accused of not keeping up. The bottleneck detector needs the cost of the
    /// work, which only the backend can separate out.
    pub cost: std::time::Duration,
}

impl ScreenFrame {
    /// A BGRA frame with its production cost recorded.
    pub fn new(width: u16, height: u16, data: Bytes, cost: std::time::Duration) -> Self {
        Self { width, height, data, format: PixelFormat::Bgra, cost }
    }

    /// As [`Self::new`], for a backend that produced something other than BGRA.
    pub fn with_format(
        width: u16,
        height: u16,
        data: Bytes,
        format: PixelFormat,
        cost: std::time::Duration,
    ) -> Self {
        Self { width, height, data, format, cost }
    }
}

/// Trait for screen capture backends. Implementations must be thread-safe.
pub trait ScreenCaptureBackend: Send + Sync {
    /// Capture the next frame. Returns `None` if no new frame is available yet.
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>>;

    /// Pointer state since the last call, if the backend tracks one.
    ///
    /// Separate from `capture_next` because the two move independently: the
    /// pointer changes on frames where the desktop does not, and vice versa.
    /// Defaults to `None` for backends with no cursor to report.
    fn take_cursor(&mut self) -> Option<super::cursor::CursorUpdate> {
        None
    }

    /// Dimensions of the frames this backend produces.
    fn frame_size(&self) -> (u16, u16);

    /// Change how much of the display is streamed, mid-session.
    ///
    /// Returns whether the request was honoured. The adaptive controller uses
    /// this to shed pixels under load and take them back afterwards, so a
    /// backend that cannot rescale simply keeps its size and the controller
    /// works the other axes instead.
    fn set_scale(&mut self, _scale: QualityScale) -> bool {
        false
    }

    /// Cap the emitted size, whatever the quality scale says.
    ///
    /// This is the guest's own viewport. Streaming more pixels than it can
    /// display is pure waste paid four times over — in bitrate, in decode, in
    /// colour conversion and in texture upload — for detail that is scaled away
    /// before anyone sees it.
    ///
    /// Returns whether it was honoured. `None` removes the cap.
    fn set_size_ceiling(&mut self, _ceiling: Option<(u16, u16)>) -> bool {
        false
    }

    /// Ask this backend to produce a different pixel format from now on.
    ///
    /// Returns whether it will. This is negotiated **once**, before the session
    /// starts, so the rest of the pipeline can treat the format as fixed — see
    /// [`negotiate_format`]. A backend that says no keeps producing BGRA, which
    /// every consumer understands.
    fn request_format(&mut self, _format: PixelFormat) -> bool {
        false
    }

    /// Whether this backend captures a real screen.
    fn is_real(&self) -> bool {
        true
    }
}

/// Settle on one pixel format for the whole session.
///
/// Both ends have to agree and both can refuse, so the answer is whatever they
/// both accept — and BGRA, which everything understands, is what that falls
/// back to. Deciding once and up front is deliberate: threading a per-frame
/// format through the pipeline would mean every consumer has to handle both,
/// and the failure mode of getting it wrong is a picture that looks broken
/// rather than an error anyone can act on.
pub fn negotiate_format(
    capturer: &mut dyn ScreenCaptureBackend,
    wanted: PixelFormat,
) -> PixelFormat {
    // # Why the answer is said out loud
    //
    // Which of these two happened decides whether the colour conversion runs on
    // the GPU or costs a full-resolution readback and a CPU pass per frame — at
    // 2560x1440 that is fourteen megabytes a frame — and until now the only
    // trace of it was a line the *converter* logs, which never runs at all if
    // the request does not reach it.
    //
    // A host reported as "Slow capture" with no GPU conversion in its log left
    // no way to tell whether the converter had refused, or had never been
    // asked. One line answers that.
    if wanted == PixelFormat::Bgra {
        debug!("Capture format: BGRA — the encoder asked for nothing else");
        return PixelFormat::Bgra;
    }
    if !capturer.request_format(wanted) {
        info!(
            "Capture format: BGRA — the encoder wanted {wanted:?} and the capture \
             backend would not produce it, so every frame is converted on the CPU"
        );
        return PixelFormat::Bgra;
    }
    debug!("Capture format: {wanted:?}, as the encoder asked");
    wanted
}

// ── Test frame generator (stub capturer) ─────────────────────

/// Generates an animated gradient. Used until a native backend is wired up.
pub struct StubCapturer {
    frame_counter: StdMutex<u64>,
    width: u16,
    height: u16,
}

impl StubCapturer {
    /// Produces frames at the *native* size. Downscaling belongs to the
    /// `Scaled` wrapper in `create_capturer`; doing it here as well applied the
    /// quality factor twice.
    pub fn new(config: &CaptureConfig) -> Self {
        let (width, height) = config.native_size();
        Self { frame_counter: StdMutex::new(0), width: width.max(1), height: height.max(1) }
    }

    fn generate_test_frame(width: u16, height: u16, counter: u64) -> ScreenFrame {
        let started = std::time::Instant::now();
        let w = width as usize;
        let h = height as usize;
        let mut data = vec![0u8; w * h * 4];
        let c = counter as usize;
        let diag = (w + h).max(1);

        for y in 0..h {
            let g = (y * 255 / h.max(1)).wrapping_add(c) as u8;
            for x in 0..w {
                let idx = (y * w + x) * 4;
                data[idx] = (x * 255 / w.max(1)).wrapping_add(c) as u8; // B
                data[idx + 1] = g; // G
                // Scale the diagonal across the frame. Casting the coordinates
                // to u8 first truncated instead of scaling, which banded the
                // red channel every 256 pixels and looked like a decode fault.
                data[idx + 2] = ((x + y) * 255 / diag).wrapping_add(c) as u8; // R
                data[idx + 3] = 255; // A
            }
        }

        // The stub never waits for anything, so all of its time is work.
        ScreenFrame::new(width, height, Bytes::from(data), started.elapsed())
    }
}

impl ScreenCaptureBackend for StubCapturer {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let counter = {
            let mut counter = self.frame_counter.lock().unwrap();
            *counter += 1;
            *counter
        };
        Ok(Some(Self::generate_test_frame(self.width, self.height, counter)))
    }

    fn frame_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn is_real(&self) -> bool {
        false
    }
}

// ── Downscaling ──────────────────────────────────────────────

/// Wraps a backend, scaling its frames down to the configured quality.
///
/// Scaling lives here rather than in each backend so a new capture backend only
/// has to produce native-resolution frames.
struct Scaled {
    inner: Box<dyn ScreenCaptureBackend>,
    width: u16,
    height: u16,
}

impl ScreenCaptureBackend for Scaled {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let Some(frame) = self.inner.capture_next()? else { return Ok(None) };
        if (frame.width, frame.height) == (self.width, self.height) {
            return Ok(Some(frame));
        }
        // `box_downscale` reads four bytes per pixel. Anything else would be
        // resampled as if it were BGRA, which produces a picture in the wrong
        // colours at a quarter of the width — so the format request below is
        // refused outright rather than guarded here. This is belt and braces.
        debug_assert_eq!(frame.format, PixelFormat::Bgra);
        // Rescaling is real capture work, so it is added to the inner cost
        // rather than replacing it.
        let started = std::time::Instant::now();
        let data = Bytes::from(box_downscale(&frame, self.width, self.height));
        Ok(Some(ScreenFrame::new(
            self.width,
            self.height,
            data,
            frame.cost + started.elapsed(),
        )))
    }

    fn frame_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn set_scale(&mut self, scale: QualityScale) -> bool {
        // Against the backend's *current* size, not a remembered one: the
        // display can change mode mid-session, and scaling a stale figure would
        // ask for a frame the source no longer produces.
        let (width, height) = scale.apply(self.inner.frame_size());
        if (width, height) != (self.width, self.height) {
            debug!("Capture rescaled to {width}x{height} ({})", scale.label());
            self.width = width;
            self.height = height;
        }
        true
    }

    /// Always BGRA.
    ///
    /// Deliberately not forwarded to the inner backend. This wrapper resamples
    /// on the CPU and can only do that for BGRA, so a backend that agreed to
    /// produce NV12 underneath would have its planes read as interleaved pixels.
    ///
    /// A backend that can rescale itself never gets wrapped in this — see
    /// [`create_capturer`] — so refusing here costs nothing on the path that
    /// matters and keeps the one that remains correct.
    fn request_format(&mut self, _format: PixelFormat) -> bool {
        false
    }

    fn is_real(&self) -> bool {
        self.inner.is_real()
    }
}

/// The largest size that fits inside `ceiling` without changing shape.
///
/// Aspect ratio is preserved rather than stretched to the guest's window: the
/// guest letterboxes what it is sent, so distorting here would make the picture
/// wrong to save nothing.
///
/// Both axes are rounded down to even numbers — H.264 needs it, and so does
/// NV12's half-resolution chroma plane.
pub fn fit_within(src: (u16, u16), ceiling: (u16, u16)) -> (u16, u16) {
    let (sw, sh) = (src.0.max(1) as u32, src.1.max(1) as u32);
    let (cw, ch) = (ceiling.0.max(2) as u32, ceiling.1.max(2) as u32);
    if sw <= cw && sh <= ch {
        return (even(sw), even(sh));
    }
    // Whichever axis is more over budget decides the scale, so the other one
    // ends up inside its own limit rather than merely closer to it.
    let by_width = sw * ch;
    let by_height = sh * cw;
    if by_width > by_height {
        (even(cw), even((sh * cw).div_ceil(sw)))
    } else {
        (even((sw * ch).div_ceil(sh)), even(ch))
    }
}

fn even(v: u32) -> u16 {
    (v.clamp(2, u16::MAX as u32) as u16) & !1
}

/// Average each destination pixel over the source pixels it covers.
///
/// Box-averaging rather than nearest-neighbour: point sampling a desktop makes
/// text shimmer as it moves, which reads as a streaming fault.
pub(crate) fn box_downscale(frame: &ScreenFrame, dst_w: u16, dst_h: u16) -> Vec<u8> {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    let mut out = vec![0u8; dw * dh * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }

    for dy in 0..dh {
        let y0 = dy * sh / dh;
        let y1 = (((dy + 1) * sh).div_ceil(dh)).min(sh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = (((dx + 1) * sw).div_ceil(dw)).min(sw).max(x0 + 1);

            let mut acc = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    for (channel, sum) in acc.iter_mut().enumerate() {
                        *sum += frame.data[i + channel] as u32;
                    }
                    count += 1;
                }
            }

            let o = (dy * dw + dx) * 4;
            let count = count.max(1);
            for (channel, sum) in acc.iter().enumerate() {
                out[o + channel] = (sum / count) as u8;
            }
        }
    }
    out
}

/// Create a screen capturer for the given configuration.
///
/// # The test pattern is not a fallback
///
/// It used to be: a platform with a real backend that failed to start got the
/// animated gradient instead, with a warning in the log and nothing at all on
/// screen to say so. That produced the worst possible outcome — a guest watching
/// a convincing moving picture that was not the host's desktop, believing they
/// were connected to it.
///
/// It happened routinely, because a second session's `DuplicateOutput` is
/// refused outright (see [`super::capture_shared`]). Now a real backend that
/// cannot start fails the session, and the stub is only ever used where it is
/// asked for by name — a synthetic display size in tests — or on a platform that
/// genuinely has no capture at all.
pub fn create_capturer(config: CaptureConfig) -> Result<Box<dyn ScreenCaptureBackend>> {
    let backend: Box<dyn ScreenCaptureBackend> = if config.native_override.is_some() {
        // An explicit display size means a synthetic one was asked for.
        Box::new(StubCapturer::new(&config))
    } else {
        #[cfg(target_os = "windows")]
        {
            // The service has no desktop to duplicate — session zero has
            // nothing on it — so it takes every frame from an agent instead of
            // wrapping a local backend that would refuse to start.
            if super::capture_follow::capture_through_agent_only() {
                Box::new(super::capture_follow::DesktopFollowing::agent_only())
            } else {
                use anyhow::Context;
                // Resolved to a real device name where the displays can be
                // enumerated, so "the primary" and its own name do not become
                // two registry entries for one display — which would mean two
                // duplications of it, and the second one failing.
                let want = config
                    .display
                    .clone()
                    .or_else(|| displays().into_iter().find(|d| d.primary).map(|d| d.name));
                let local = super::capture_shared::attach(want)
                    .context("could not start screen capture")?;
                // Wrapped so a consent prompt does not end the picture. See
                // `capture_follow`: everything above this sees one capturer.
                Box::new(super::capture_follow::DesktopFollowing::new(local))
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            warn!("No screen capture backend on this platform — streaming the test pattern");
            Box::new(StubCapturer::new(&config))
        }
    };

    // The quality scale applies to whatever the backend actually produces,
    // which may differ from the display we queried (a mode change, or the stub).
    let native = backend.frame_size();
    let (width, height) = config.quality.apply(native);

    debug!(
        "Capture {native:?} at {} -> {width}x{height} @ {}fps (real={})",
        config.quality.label(),
        config.fps,
        backend.is_real()
    );

    if (width, height) == native {
        return Ok(backend);
    }

    // A backend that can rescale itself does it far better than this wrapper
    // can: on the DXGI path the video processor resizes in the same pass as the
    // colour conversion, for nothing, while `box_downscale` is a single-threaded
    // resample of every source pixel on the capture thread. That was the "Slow
    // capture" the host reported the moment the controller dropped below full
    // resolution — which is exactly when it could least afford it.
    let mut backend = backend;
    if backend.set_scale(config.quality) && backend.frame_size() == (width, height) {
        return Ok(backend);
    }
    Ok(Box::new(Scaled { inner: backend, width, height }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(native: (u16, u16), quality: QualityScale) -> CaptureConfig {
        CaptureConfig {
            display: None,
            fps: 30,
            quality,
            show_cursor: false,
            native_override: Some(native),
        }
    }

    #[test]
    fn native_quality_streams_the_whole_display() {
        let cfg = config_for((2560, 1440), QualityScale::Native);
        assert_eq!(cfg.dimensions(), (2560, 1440));
    }

    #[test]
    fn quality_scales_both_axes() {
        let native = (1920, 1080);
        assert_eq!(config_for(native, QualityScale::High).dimensions(), (1440, 810));
        assert_eq!(config_for(native, QualityScale::Medium).dimensions(), (960, 540));
        assert_eq!(config_for(native, QualityScale::Low).dimensions(), (640, 360));
    }

    #[test]
    fn scaled_dimensions_are_always_even() {
        // Odd dimensions break chroma subsampling in every real codec.
        for native in [(1366, 768), (1365, 767), (3841, 2161), (100, 33)] {
            for quality in QualityScale::ALL {
                let (w, h) = quality.apply(native);
                assert_eq!(w % 2, 0, "{native:?} at {quality:?} gave odd width {w}");
                assert_eq!(h % 2, 0, "{native:?} at {quality:?} gave odd height {h}");
                assert!(w >= 2 && h >= 2);
            }
        }
    }

    #[test]
    fn tiny_displays_do_not_collapse_to_nothing() {
        let (w, h) = QualityScale::Low.apply((4, 4));
        assert!(w >= 2 && h >= 2, "got {w}x{h}");
    }

    /// Build the stub explicitly. `create_capturer` prefers the real backend
    /// where one exists, so tests about the test pattern must not go through it.
    fn stub(native: (u16, u16)) -> StubCapturer {
        StubCapturer::new(&config_for(native, QualityScale::Native))
    }

    #[test]
    fn the_stub_produces_frames_at_the_native_size() {
        // Regression: the stub also applied the quality factor, so wrapping it
        // in the scaler squared the reduction.
        let mut cap = stub((1920, 1080));
        assert_eq!(cap.frame_size(), (1920, 1080));

        let frame = cap.capture_next().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (1920, 1080));
        assert_eq!(frame.data.len(), 1920 * 1080 * 4);
    }

    #[test]
    fn scaling_is_applied_exactly_once() {
        let cfg = config_for((1920, 1080), QualityScale::Medium);
        let mut scaled = Scaled {
            inner: Box::new(StubCapturer::new(&cfg)),
            width: cfg.dimensions().0,
            height: cfg.dimensions().1,
        };

        assert_eq!(scaled.frame_size(), (960, 540));
        let frame = scaled.capture_next().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (960, 540));
        assert_eq!(frame.data.len(), 960 * 540 * 4);
    }

    #[test]
    fn downscaling_averages_rather_than_point_samples() {
        // A 2x2 block of known values must average, not pick one pixel —
        // point sampling makes text shimmer as it moves.
        let src = ScreenFrame::new(
            2,
            2,
            Bytes::from(vec![
                0, 0, 0, 255, 100, 100, 100, 255, // row 0
                100, 100, 100, 255, 200, 200, 200, 255, // row 1
            ]),
            Default::default(),
        );
        let out = box_downscale(&src, 1, 1);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 100, "expected the mean of 0/100/100/200");
        assert_eq!(out[3], 255, "alpha should survive");
    }

    #[test]
    fn downscaling_preserves_a_flat_colour() {
        let src = ScreenFrame::new(8, 8, Bytes::from(vec![0x40u8; 8 * 8 * 4]), Default::default());
        let out = box_downscale(&src, 3, 3);
        assert!(out.iter().all(|b| *b == 0x40), "averaging a flat image must not drift");
    }

    #[test]
    fn the_test_pattern_has_no_banding_across_a_wide_frame() {
        // Regression: the red channel was computed from `x as u8`, which wrapped
        // every 256 pixels and produced seams that looked like decode corruption.
        let mut cap = stub((640, 8));
        let frame = cap.capture_next().unwrap().unwrap();

        let row: Vec<u8> = (0..640).map(|x| frame.data[x * 4 + 2]).collect();
        let biggest_jump =
            row.windows(2).map(|w| (w[0] as i16 - w[1] as i16).abs()).max().unwrap();
        assert!(biggest_jump <= 2, "red channel jumps by {biggest_jump} — banding is back");
    }

    #[test]
    fn frames_animate_between_captures() {
        let mut cap = stub((16, 16));
        let a = cap.capture_next().unwrap().unwrap();
        let b = cap.capture_next().unwrap().unwrap();
        assert_ne!(a.data, b.data, "stub frames must change so rendering bugs are visible");
    }

    #[test]
    fn the_stub_reports_itself_as_not_real() {
        assert!(!stub((640, 360)).is_real(), "the UI needs to know this is not a real screen");
    }

    #[test]
    fn detected_display_is_plausible() {
        let (w, h) = primary_display_size();
        assert!(w >= 640 && h >= 480, "implausible display size {w}x{h}");
    }
}

#[cfg(test)]
mod fit_tests {
    use super::fit_within;

    #[test]
    fn a_ceiling_larger_than_the_source_changes_nothing() {
        // Never upscale to fill the guest's window: sending more pixels than the
        // host has invents detail and costs bitrate to do it.
        assert_eq!(fit_within((1280, 800), (2560, 1600)), (1280, 800));
        assert_eq!(fit_within((1920, 1080), (1920, 1080)), (1920, 1080));
    }

    #[test]
    fn the_shape_of_the_picture_survives() {
        // 16:10 host into a 16:9 window: the height binds, and the result is
        // still 16:10 rather than stretched to fill.
        let (w, h) = fit_within((2560, 1600), (1920, 1080));
        assert_eq!((w, h), (1728, 1080));
        let ratio = w as f32 / h as f32;
        assert!((ratio - 1.6).abs() < 0.01, "got {ratio}, expected 16:10");

        // And the other way: a tall window against a wide host.
        let (w, h) = fit_within((2560, 1600), (800, 2000));
        assert_eq!(w, 800);
        assert!(h <= 2000);
        assert!((w as f32 / h as f32 - 1.6).abs() < 0.02);
    }

    #[test]
    fn the_result_always_fits_inside_the_ceiling() {
        // The binding axis has to be the one that is *more* over budget, or the
        // other one comes out over its own limit.
        for src in [(2560u16, 1600u16), (1920, 1080), (3840, 2160), (1024, 768)] {
            for ceiling in [(1280u16, 760u16), (640, 480), (1920, 200), (100, 1200)] {
                let (w, h) = fit_within(src, ceiling);
                assert!(
                    w <= ceiling.0.max(2) && h <= ceiling.1.max(2),
                    "{src:?} into {ceiling:?} gave {w}x{h}"
                );
            }
        }
    }

    #[test]
    fn both_axes_are_always_even_and_never_zero() {
        // H.264 needs even dimensions and NV12 needs them for its chroma plane.
        // A window dragged down to nothing must not ask for a 0x0 stream.
        for ceiling in [(1u16, 1u16), (3, 7), (0, 0), (1281, 761)] {
            let (w, h) = fit_within((2560, 1600), ceiling);
            assert_eq!(w % 2, 0, "{ceiling:?} gave an odd width {w}");
            assert_eq!(h % 2, 0, "{ceiling:?} gave an odd height {h}");
            assert!(w >= 2 && h >= 2, "{ceiling:?} gave {w}x{h}");
        }
    }

    #[test]
    fn a_typical_window_cuts_the_pixel_count_by_three_quarters() {
        // The whole point, as a number: this is what the guest stops having to
        // decode, convert and upload every frame.
        let (w, h) = fit_within((2560, 1600), (1280, 760));
        let before = 2560u32 * 1600;
        let after = w as u32 * h as u32;
        assert!(
            after * 4 <= before,
            "{w}x{h} is {after} pixels against {before} — expected a quarter or less"
        );
    }

}

#[cfg(test)]
mod display_tests {
    use super::{displays, primary_display_size};

    /// Whatever this machine reports, it has to be self-consistent.
    ///
    /// # Why the assertions are shaped like this
    ///
    /// The list depends on hardware nobody writing the test has, so asserting a
    /// count or a size would only pin down the machine it was written on. What
    /// can be asserted is that the answers agree with each other: ids that name
    /// one display each, exactly one display at the origin, and no display
    /// claiming a size it cannot have.
    ///
    /// On a machine with no display backend the list is empty, and empty is a
    /// legitimate answer — it means "cannot say", which callers are required to
    /// treat as one unknown screen rather than as none.
    #[test]
    fn the_displays_this_machine_reports_agree_with_each_other() {
        let found = displays();
        if found.is_empty() {
            return; // No backend, or no display. Nothing to be inconsistent about.
        }

        let ids: std::collections::HashSet<u32> = found.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), found.len(), "two displays share an id: {found:?}");

        let primaries = found.iter().filter(|d| d.primary).count();
        assert_eq!(
            primaries, 1,
            "exactly one display sits at the origin of the virtual desktop, not {primaries}: \
             {found:?}"
        );

        for display in &found {
            assert!(display.width > 0 && display.height > 0, "{display:?} has no extent");
            assert!(!display.name.is_empty(), "{display:?} has no name to put in a menu");
        }

        // The primary is the one at the origin, and nothing else may be there.
        let primary = found.iter().find(|d| d.primary).expect("checked above");
        assert_eq!((primary.x, primary.y), (0, 0), "the primary display is not at the origin");

        // Displays do not overlap: two occupying the same pixel would make
        // "which display is this position on" unanswerable, which is what the
        // input mapping will need.
        for (i, a) in found.iter().enumerate() {
            for b in found.iter().skip(i + 1) {
                let apart = a.x + i32::from(a.width) <= b.x
                    || b.x + i32::from(b.width) <= a.x
                    || a.y + i32::from(a.height) <= b.y
                    || b.y + i32::from(b.height) <= a.y;
                assert!(apart, "{} and {} overlap: {a:?} {b:?}", a.name, b.name);
            }
        }

        eprintln!("this machine reports {} display(s):", found.len());
        for display in &found {
            eprintln!("  {display:?}");
        }
    }

    /// The reported primary matches the one the rest of the code assumes.
    ///
    /// `primary_display_size` and capture both mean the primary display, and
    /// they reach it by different routes — `EnumDisplaySettings` and DXGI. A
    /// disagreement between them is how a pointer ends up scaled against one
    /// display and injected into another.
    #[test]
    fn the_primary_display_is_the_same_one_both_ways() {
        let Some(primary) = displays().into_iter().find(|d| d.primary) else {
            return; // Nothing enumerated; nothing to cross-check.
        };
        let (width, height) = primary_display_size();
        assert_eq!(
            (primary.width, primary.height),
            (width, height),
            "DXGI and EnumDisplaySettings disagree about the primary display"
        );
    }
}
