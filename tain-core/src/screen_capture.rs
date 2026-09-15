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

/// A capture failure that has something to be done about it, named rather than
/// worded.
///
/// # Why this is not simply a sentence
///
/// The remedy for a wrong-GPU failure is four paragraphs of advice about the
/// *host's* Windows settings, and the only screen it ever reaches is the
/// *guest's* — the host logs it and hands it to the session, which sends it on.
/// Written as prose it is therefore prose in the wrong language by
/// construction, which is the same fault [`super::protocol::SessionEnd`] was
/// carrying and for the same reason.
///
/// So it travels as a key and its placeholder values, and the machine that
/// shows it is the machine that picks the words. `Display` renders English,
/// because the only thing that reads it locally is the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remedy {
    /// One of [`super::protocol::end_reason::wrong_gpu`], which is also what
    /// travels as `SessionEnd::reason`.
    pub code: &'static str,
    /// Values for that sentence's `{placeholders}`.
    pub args: Vec<(String, String)>,
}

impl Remedy {
    pub fn new(code: &'static str, args: &[(&str, &str)]) -> Self {
        let args = args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Self { code, args }
    }

    /// The remedy inside an error chain, if there is one.
    ///
    /// Looked for by type rather than by text, so it survives however many
    /// layers of context the session-start path puts on top of it.
    pub fn in_chain(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|link| link.downcast_ref::<Self>())
    }

    /// Where the words live. The same shape the guest builds from a
    /// `SessionEnd`, and the reason both ends read the same sentence.
    pub fn key(&self) -> String {
        format!("end.{}", self.code)
    }

    /// The args as `i18n::format` wants them.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }
}

impl std::fmt::Display for Remedy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&super::i18n::format_in_english(&self.key(), &self.pairs()))
    }
}

impl std::error::Error for Remedy {}

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
        crate::i18n::text(match self {
            Self::Native => "quality.native",
            Self::High => "quality.high",
            Self::Medium => "quality.medium",
            Self::Low => "quality.low",
        })
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

/// Adapters that exist to present a display nothing is plugged into.
///
/// # Why a list of names and not a property
///
/// There is no flag for this. An indirect display driver presents an adapter
/// that looks like any other — `DXGI_ADAPTER_FLAG_SOFTWARE` means WARP, not
/// this, and the memory figures that look distinctive on a discrete GPU look
/// exactly like an integrated one. What actually identifies these is who wrote
/// them.
///
/// So this is a list, matched case-insensitively as a substring, and it will be
/// incomplete. Being wrong here costs a label, not a capture: an unrecognised
/// virtual display works exactly as a real one does, which is the safe
/// direction. `capture_dxgi::tests::what_owns_each_display` prints adapter
/// descriptions, which is how to find a name that belongs here.
#[cfg(target_os = "windows")]
const VIRTUAL_ADAPTERS: &[&str] = &[
    // The MIT driver descended from Microsoft's IddSampleDriver, and the sample
    // itself. Sunshine and Parsec users mostly have one of these.
    "iddsampledriver",
    "idd sample",
    "virtual display driver",
    "mttvdd",
    // Parsec's own, installed standalone by a good number of people.
    "parsec virtual display",
    // Long-standing USB-over-IP style virtual monitors.
    "usbmmidd",
    "amyuni",
    // Generic enough to be worth having, specific enough not to match a GPU.
    "indirect display",
    "virtual monitor",
];

/// Whether an adapter description names a driver that presents virtual screens.
///
/// See [`VIRTUAL_ADAPTERS`] for why this is a name match.
///
/// The array is what both callers already hold: DXGI's `Description` and GDI's
/// `DISPLAY_DEVICEW::DeviceString` are the same shape, which is why
/// [`super::virtual_display`] can ask the same question of a display that is
/// not attached and therefore has no DXGI output at all.
#[cfg(target_os = "windows")]
pub(crate) fn is_virtual_adapter(description: &[u16; 128]) -> bool {
    let end = description.iter().position(|&c| c == 0).unwrap_or(description.len());
    let name = String::from_utf16_lossy(&description[..end]).to_ascii_lowercase();
    VIRTUAL_ADAPTERS.iter().any(|known| name.contains(known))
}

/// Which of the host's screens a session is watching.
///
/// Names rather than the wire's numeric ids: an id is an enumeration index and
/// renumbers when a monitor is plugged in, while a device name does not. The
/// translation happens once, where the request arrives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DisplayTarget {
    /// Whichever display sits at the origin of the virtual desktop. What a
    /// session that has not asked for anything else gets.
    #[default]
    Primary,
    /// One named display, as [`displays`] reports it.
    One(String),
    /// Every display, composed into one picture laid out as the host has them.
    /// See [`super::capture_wall`].
    All,
    /// One application window, by handle, and nothing around it.
    ///
    /// Only the compositor can produce this — a window that is partly covered
    /// is not in the desktop image to be cropped out of. See
    /// [`super::window_capture`] and [`super::capture_wgc`].
    Window(u64),
    /// Several application windows, tiled into one picture, and nothing else.
    ///
    /// The window answer to [`Self::All`], and laid out differently for a
    /// reason. Screens are composed where the host has them because they *are*
    /// somewhere: they sit beside each other on a desk and the arrangement is a
    /// fact. Windows overlap, in a stack whose order changes as somebody works,
    /// so composing them by position would hide the ones underneath — two
    /// maximised windows would show as one. A grid shows all of them, which is
    /// the entire reason for asking.
    ///
    /// See [`super::capture_wall::Wall::of_windows`].
    Windows(Vec<u64>),
}

/// What the monitor calls itself, for a person choosing between them.
///
/// # Why not the device name
///
/// `\\.\DISPLAY2` is an index into a list nobody can see, and it renumbers.
/// Somebody deciding which screens to share needs to recognise the monitor on
/// their desk, and what does that is the name burned into its EDID — "DELL
/// U2717D", "LG HDR 4K".
///
/// `None` where the monitor did not say, which is normal for a virtual display
/// and for panels with an empty EDID string. The caller falls back to position
/// and size, which is what it used before this existed.
#[cfg(target_os = "windows")]
fn monitor_name(device_name: &[u16; 32]) -> Option<String> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let (mut path_count, mut mode_count) = (0, 0);
    // SAFETY: both out-parameters are owned locals.
    if unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    } != ERROR_SUCCESS
    {
        return None;
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: both buffers are exactly the length just asked for, and the counts
    // are passed by pointer so the call can shorten them.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    paths.truncate(path_count as usize);

    for path in paths {
        // The source says which `\\.\DISPLAYn` this path drives; the target,
        // which physical monitor is on the end of it. Both are needed, because
        // the question is asked by device name and answered by monitor name.
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the header describes the struct it is embedded in, which is a
        // local and outlives the call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        if source.viewGdiDeviceName != *device_name {
            continue;
        }

        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: as above.
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
            return None;
        }

        let name = &target.monitorFriendlyDeviceName;
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let name = String::from_utf16_lossy(&name[..end]).trim().to_string();
        return (!name.is_empty()).then_some(name);
    }
    None
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
                // Asked once per adapter rather than per output: every display
                // on an indirect-display adapter is a virtual one.
                let virtual_display = adapter
                    .GetDesc1()
                    .is_ok_and(|desc| is_virtual_adapter(&desc.Description));

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
                        monitor: monitor_name(&desc.DeviceName),
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
                        virtual_display,
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
    /// Which of the host's screens to capture. See [`DisplayTarget`].
    pub display: DisplayTarget,
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
            display: Default::default(),
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

    /// Whether what this is capturing has been rearranged underneath it.
    ///
    /// Only the composed wall answers yes. It is a picture of *where the screens
    /// are*, decided when it was built — so a monitor moved in Display Settings,
    /// or plugged in, leaves it drawing an arrangement that no longer exists.
    /// The pointer maps through that arrangement, so a stale one puts clicks
    /// where the user did not put them.
    ///
    /// It cannot repair itself: its panels are attached to particular displays.
    /// Saying so is what gets it rebuilt.
    fn layout_changed(&self) -> bool {
        false
    }

    /// How a guest's coordinates map back to this machine, when one rectangle
    /// will not do.
    ///
    /// `None` from everything that produces a picture of one region of the
    /// desktop — a screen, a window, a wall of screens — because a uniform
    /// scale of one rectangle is what the injector already assumes.
    ///
    /// `Some` only from a composite of windows, which is an arrangement this
    /// end invented out of sources scattered across the desk. See
    /// [`super::input_inject::Tile`].
    fn input_tiles(&self) -> Option<Vec<super::input_inject::Tile>> {
        None
    }

    /// Emit only sizes whose axes are multiples of these.
    ///
    /// The encoder's constraint, not the capturer's — see
    /// [`crate::encoder::VideoEncoder::size_granularity`] for what asks for it
    /// and why one encoder on one machine refuses sizes another takes happily.
    ///
    /// Returns whether it was honoured.
    fn set_size_granularity(&mut self, _granularity: (u16, u16)) -> bool {
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

    /// What the frames coming out of here are laid out as, right now.
    ///
    /// Asked by [`super::capture_shared`], where a second session joining a
    /// capture that already exists needs to know whether the format it wants is
    /// the one being produced — the answer decides whether it can join or has
    /// to take what is on offer.
    ///
    /// Defaults to BGRA, because that is what a backend which refuses
    /// [`Self::request_format`] necessarily produces.
    fn current_format(&self) -> PixelFormat {
        PixelFormat::Bgra
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
    // At `info` like the refusal above, and for the same reason: which of the
    // two happened decides whether the resize and the colour pass run on the
    // GPU or on the CPU, and that is the first question to ask about a session
    // that will not reach its frame rate. A line each way means the log answers
    // it rather than only recording the bad case.
    info!("Capture format: {wanted:?}, as the encoder asked — converted on the GPU");
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
    /// Everything the emitted size is derived from, so it can be derived again
    /// when any one of them moves.
    ///
    /// It used to keep only the answer, and each setter recomputed from
    /// whatever it happened to know — which is how a ceiling and a granularity
    /// came to be accepted by this type and then quietly dropped.
    scale: QualityScale,
    ceiling: Option<(u16, u16)>,
    granularity: (u16, u16),
}

impl Scaled {
    /// Work the emitted size out again from the backend's current size.
    ///
    /// Against the backend's *current* size, not a remembered one: the display
    /// can change mode mid-session, and scaling a stale figure would ask for a
    /// frame the source no longer produces.
    fn recompute(&mut self) {
        let scaled = self.scale.apply(self.inner.frame_size());
        let fitted = match self.ceiling {
            Some(ceiling) => fit_within(scaled, ceiling),
            None => scaled,
        };
        let (width, height) = snap_to(fitted, self.granularity);
        if (width, height) != (self.width, self.height) {
            debug!("Capture rescaled to {width}x{height} ({})", self.scale.label());
            self.width = width;
            self.height = height;
        }
    }
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
        self.scale = scale;
        self.recompute();
        true
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        self.ceiling = ceiling;
        self.recompute();
        true
    }

    fn set_size_granularity(&mut self, granularity: (u16, u16)) -> bool {
        self.granularity = granularity;
        self.recompute();
        true
    }

    /// Forwarded, with the tiles moved into the size this actually emits.
    ///
    /// # Why this is here for a case that should not arise
    ///
    /// Only a composite of windows has tiles, and it scales itself — so
    /// [`create_capturer`] never wraps one in this. "Never" of that kind is a
    /// property of code somewhere else, and the failure if it stops holding is
    /// silent: the tiles would describe a picture larger than the one being
    /// sent, and every click would land short of where the guest aimed.
    fn input_tiles(&self) -> Option<Vec<super::input_inject::Tile>> {
        let tiles = self.inner.input_tiles()?;
        let (from_w, from_h) = self.inner.frame_size();
        let across = |value: u16, from: u16, to: u16| -> u16 {
            (u32::from(value) * u32::from(to) / u32::from(from.max(1))).min(u32::from(u16::MAX))
                as u16
        };
        Some(
            tiles
                .into_iter()
                .map(|tile| {
                    let (x, y, w, h) = tile.picture;
                    super::input_inject::Tile {
                        picture: (
                            across(x, from_w, self.width),
                            across(y, from_h, self.height),
                            across(w, from_w, self.width),
                            across(h, from_h, self.height),
                        ),
                        ..tile
                    }
                })
                .collect(),
        )
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

/// Round an emitted size down to what an encoder will actually take.
///
/// # Why this is not just rounding both axes
///
/// AMD's AV1 transform wants width in multiples of sixty-four. Rounding each
/// axis on its own would take 1382x864 to 1344x832 — the width loses 38 pixels
/// and the height 32, which are different fractions, and the picture arrives at
/// the guest stretched. The guest draws a frame at the size the frame says it
/// is; nothing downstream knows what shape it was meant to be.
///
/// So only the width is rounded, and the height follows from it. The error left
/// is half the height's granularity — under one percent at any size worth
/// streaming — against nearly three percent for the naive version.
///
/// Never rounds up: this is applied after a ceiling, and growing past it would
/// undo the cap that a guest's window, the wall's own limit, or an encoder's
/// maximum had just imposed.
pub fn snap_to(size: (u16, u16), granularity: (u16, u16)) -> (u16, u16) {
    let (gw, gh) = (granularity.0.max(1) as u32, granularity.1.max(1) as u32);
    let (w, h) = (size.0.max(1) as u32, size.1.max(1) as u32);

    let out_w = (w / gw * gw).max(gw);
    // From the shape that was asked for, at the width that survived rounding.
    let ideal_h = (out_w * h).div_ceil(w);
    // Nearest rather than down, so the shape is as close as the grid allows,
    // then held inside the size we were given.
    let out_h = ((ideal_h + gh / 2) / gh * gh).clamp(gh, (h / gh * gh).max(gh));

    (out_w.min(u16::MAX as u32) as u16, out_h.min(u16::MAX as u32) as u16)
}

/// Average each destination pixel over the source pixels it covers.
///
/// Box-averaging rather than nearest-neighbour: point sampling a desktop makes
/// text shimmer as it moves, which reads as a streaming fault.
///
/// # Why this is worth threading
///
/// It is the whole cost of the capture path that cannot hand its resize to the
/// GPU: a window captured as BGRA, and every panel of a composite. Measured on
/// one thread at **22 ms** for a 2446x1516 window and **33 ms** for a 4K one —
/// against a 16.7 ms budget at 60 fps, so the frame rate could not reach its
/// target however idle the machines were. A single shared window was reported
/// streaming at sixteen frames a second, which is exactly where the adaptive
/// controller settles when capture costs this much.
///
/// The rows of the output are independent, which is what makes this a band per
/// worker and no synchronisation beyond the join — the same shape as the NV12
/// conversion in `codec_mf`, and for the same reason.
pub(crate) fn box_downscale(frame: &ScreenFrame, dst_w: u16, dst_h: u16) -> Vec<u8> {
    /// Below this, threading costs more than it saves: spawning is tens of
    /// microseconds and a small resize is not much more than that.
    const ROWS_PER_WORKER: usize = 64;

    /// Source pixels one worker is worth asking for.
    ///
    /// # Why a resize does not simply take every core
    ///
    /// It is not the only one running. A composite resizes every panel at once,
    /// on a thread each, so a core per worker meant a sixteen-thread machine
    /// spawning ninety-six threads per composed frame — reported as every core
    /// pinned, with no gain in frame rate, because there was none to be had.
    ///
    /// Asking for workers in proportion to the *work* settles both ends of
    /// that: a panel in a grid is small and asks for one or two, while a 4K
    /// window on its own asks for eight and gets its resize back under the
    /// frame budget. The whole machine is never handed to one caller, and no
    /// caller is capped for the sake of a neighbour it does not have.
    const PIXELS_PER_WORKER: usize = 1_000_000;

    /// However much work there is. Beyond this the measurement flattens and the
    /// spawns start costing more than the split saves.
    const MAX_WORKERS: usize = 8;

    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (dw, dh) = (dst_w as usize, dst_h as usize);
    let mut out = vec![0u8; dw * dh * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_WORKERS)
        .min((sw * sh).div_ceil(PIXELS_PER_WORKER))
        .min(dh.div_ceil(ROWS_PER_WORKER))
        .max(1);
    let band = dh.div_ceil(workers);
    let source = &frame.data[..];

    if workers == 1 {
        downscale_band(source, (sw, sh), (dw, dh), 0, &mut out);
        return out;
    }

    std::thread::scope(|scope| {
        for (index, rows) in out.chunks_mut(band * dw * 4).enumerate() {
            scope.spawn(move || downscale_band(source, (sw, sh), (dw, dh), index * band, rows));
        }
    });
    out
}

/// Resample the destination rows starting at `first_row` into `out`.
fn downscale_band(
    source: &[u8],
    (sw, sh): (usize, usize),
    (dw, dh): (usize, usize),
    first_row: usize,
    out: &mut [u8],
) {
    for local in 0..out.len() / (dw * 4) {
        let dy = first_row + local;
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
                        *sum += source[i + channel] as u32;
                    }
                    count += 1;
                }
            }

            let o = (local * dw + dx) * 4;
            let count = count.max(1);
            for (channel, sum) in acc.iter().enumerate() {
                out[o + channel] = (sum / count) as u8;
            }
        }
    }
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
/// A capturer that produces nothing, for a session with nothing to capture.
///
/// Used while a shared window is minimised. The alternative — keeping the real
/// capturer — is a compositor session held open against a window the compositor
/// has stopped drawing, and the alternative to *that* is tearing the session
/// down, which turns "the host minimised it" into "the connection dropped".
pub fn create_stub(config: &CaptureConfig) -> Box<dyn ScreenCaptureBackend> {
    Box::new(StubCapturer::new(config))
}

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
                let local: Box<dyn ScreenCaptureBackend> = match &config.display {
                    DisplayTarget::All => {
                        let all = displays();
                        anyhow::ensure!(
                            !all.is_empty(),
                            "this host cannot enumerate its displays, so it cannot compose them"
                        );
                        Box::new(super::capture_wall::Wall::new(&all)?)
                    }
                    // Only the compositor holds a window's own pixels, so there
                    // is no fallback here and none is wanted: a window share
                    // that quietly became a screen share would be a promise
                    // broken silently. See `window_capture`.
                    DisplayTarget::Window(id) => Box::new(
                        super::capture_wgc::WgcCapturer::for_window(*id)
                            .context("could not capture that window")?,
                    ),
                    // Asked about again here rather than carried in the target:
                    // the grid needs each window's size, and a size recorded
                    // when the grant was made is the size it was then.
                    DisplayTarget::Windows(ids) => {
                        let known: Vec<_> = ids
                            .iter()
                            .filter_map(|id| super::window_capture::info_of(*id))
                            .collect();
                        anyhow::ensure!(
                            !known.is_empty(),
                            "none of the shared windows are still open"
                        );
                        Box::new(
                            super::capture_wall::Wall::of_windows(&known)
                                .context("could not compose those windows")?,
                        )
                    }
                    chosen => {
                        let want = match chosen {
                            DisplayTarget::One(name) => Some(name.clone()),
                            _ => displays().into_iter().find(|d| d.primary).map(|d| d.name),
                        };
                        super::capture_shared::attach(want)
                            .context("could not start screen capture")?
                    }
                };
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
    //
    // # Why `set_scale` saying yes is the whole answer
    //
    // This also required the backend to come back at exactly the size worked
    // out above, on the reasoning that a backend claiming to have scaled and
    // then not doing it should not be believed. But a backend is entitled to a
    // different answer: the composed wall caps itself at what an encoder will
    // take, and a display capping to the guest's viewport lands wherever that
    // leaves it. Neither is a lie, and both failed the equality.
    //
    // So the wall was wrapped in this after having already scaled itself, and
    // the scale was applied twice — 4480x1600 came out as 2520x900, which is
    // 75% of 75%, resampled on the CPU for the privilege. Worse, `Scaled` is
    // what the rest of the session then talks to, and until a moment ago it
    // answered "not honoured" to the encoder's size granularity and to the
    // guest's viewport, both silently.
    //
    // `set_scale` returning true is the backend saying it handled it. That is
    // the contract, the default is `false` for backends that cannot, and taking
    // it at its word is what stops this wrapper being layered on top of work
    // that is already done.
    let mut backend = backend;
    if backend.set_scale(config.quality) {
        return Ok(backend);
    }
    Ok(Box::new(Scaled {
        inner: backend,
        width,
        height,
        scale: config.quality,
        ceiling: None,
        granularity: (2, 2),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Threading the resize does not change the picture it produces.
    ///
    /// # Why this needs its own test
    ///
    /// The bands are disjoint slices of the output, so nothing is shared and
    /// nothing needs a lock — but each worker addresses its rows twice, once as
    /// a position in the *source* and once as an offset into its own slice, and
    /// getting that pair wrong is a picture that is subtly striped rather than a
    /// crash. The existing tests use frames small enough to stay on one thread,
    /// so they would not see it.
    #[test]
    fn resizing_in_bands_matches_resizing_in_one_pass() {
        /// The obvious implementation, kept here as the thing to agree with.
        fn reference(frame: &ScreenFrame, dw: usize, dh: usize) -> Vec<u8> {
            let (sw, sh) = (frame.width as usize, frame.height as usize);
            let mut out = vec![0u8; dw * dh * 4];
            for dy in 0..dh {
                let y0 = dy * sh / dh;
                let y1 = (((dy + 1) * sh).div_ceil(dh)).min(sh).max(y0 + 1);
                for dx in 0..dw {
                    let x0 = dx * sw / dw;
                    let x1 = (((dx + 1) * sw).div_ceil(dw)).min(sw).max(x0 + 1);
                    let mut acc = [0u32; 4];
                    let mut count = 0u32;
                    for sy in y0..y1 {
                        for sx in x0..x1 {
                            let i = (sy * sw + sx) * 4;
                            for (c, sum) in acc.iter_mut().enumerate() {
                                *sum += frame.data[i + c] as u32;
                            }
                            count += 1;
                        }
                    }
                    for (c, sum) in acc.iter().enumerate() {
                        out[(dy * dw + dx) * 4 + c] = (sum / count.max(1)) as u8;
                    }
                }
            }
            out
        }

        // Tall enough to be split across workers, and content that varies in
        // both axes so a band drawn from the wrong rows cannot match by luck.
        let (sw, sh) = (640usize, 700usize);
        let pixels: Vec<u8> = (0..sw * sh * 4)
            .map(|i| {
                let px = i / 4;
                let (x, y) = (px % sw, px / sw);
                ((x * 7 + y * 13 + i % 4) % 251) as u8
            })
            .collect();
        let frame = ScreenFrame::new(
            sw as u16,
            sh as u16,
            bytes::Bytes::from(pixels),
            std::time::Duration::ZERO,
        );

        // Sizes that do not divide evenly, so the band boundaries land mid-row
        // of the source and the rounding has to agree as well.
        for (dw, dh) in [(320usize, 350usize), (211, 173), (639, 699)] {
            let threaded = box_downscale(&frame, dw as u16, dh as u16);
            assert_eq!(
                threaded,
                reference(&frame, dw, dh),
                "resizing to {dw}x{dh} in bands did not match one pass"
            );
        }
    }

    /// What resizing a frame costs on the CPU, at the sizes sessions use.
    ///
    /// Reported rather than asserted. A window session was measured at sixteen
    /// frames a second, which is where the adaptive controller settles when
    /// capture spends forty to sixty milliseconds a frame — and this is the one
    /// thing on that path that could plausibly cost it. Run in release; a debug
    /// build says nothing useful about a per-pixel loop.
    #[test]
    #[ignore = "a measurement — run explicitly, and in release"]
    fn report_what_resizing_a_frame_costs() {
        for (from, to) in [
            ((2446u16, 1516u16), (1200u16, 750u16)),
            ((2560, 1440), (1280, 720)),
            ((3840, 2160), (1920, 1080)),
        ] {
            let pixels = vec![0x7Fu8; usize::from(from.0) * usize::from(from.1) * 4];
            let frame = ScreenFrame::new(
                from.0,
                from.1,
                bytes::Bytes::from(pixels),
                std::time::Duration::ZERO,
            );

            let started = std::time::Instant::now();
            const ROUNDS: u32 = 10;
            for _ in 0..ROUNDS {
                let _ = box_downscale(&frame, to.0, to.1);
            }
            println!(
                "  {}x{} -> {}x{}: {:.1} ms each",
                from.0,
                from.1,
                to.0,
                to.1,
                started.elapsed().as_secs_f64() * 1000.0 / f64::from(ROUNDS),
            );
        }
    }

    fn config_for(native: (u16, u16), quality: QualityScale) -> CaptureConfig {
        CaptureConfig {
            display: Default::default(),
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
        let mut scaled = scaled_over(&cfg, QualityScale::Medium);

        assert_eq!(scaled.frame_size(), (960, 540));
        let frame = scaled.capture_next().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (960, 540));
        assert_eq!(frame.data.len(), 960 * 540 * 4);

        // And still once when the controller asks again for what is already in
        // force. This wrapper used to scale its *own* output — so a wall that
        // had already scaled itself, wrapped here and then told the scale,
        // emitted 75% of 75%: 4480x1600 arrived at the encoder as 2520x900,
        // resampled on the CPU for the privilege.
        scaled.set_scale(QualityScale::Medium);
        assert_eq!(scaled.frame_size(), (960, 540), "the scale was applied twice");
    }

    fn scaled_over(cfg: &CaptureConfig, scale: QualityScale) -> Scaled {
        Scaled {
            inner: Box::new(StubCapturer::new(cfg)),
            width: cfg.dimensions().0,
            height: cfg.dimensions().1,
            scale,
            ceiling: None,
            granularity: (2, 2),
        }
    }

    /// Both of these reached this wrapper and were dropped without a word.
    ///
    /// The trait's defaults answer "not honoured" and do nothing, so a backend
    /// wrapped here streamed at the quality slider's fraction whatever the
    /// guest's window was, and on whatever size grid, however loudly the
    /// encoder had said which sizes it takes.
    #[test]
    fn the_ceiling_and_the_grid_reach_this_wrapper_too() {
        let cfg = config_for((1920, 1080), QualityScale::Native);
        let mut scaled = scaled_over(&cfg, QualityScale::Native);
        assert_eq!(scaled.frame_size(), (1920, 1080));

        assert!(scaled.set_size_ceiling(Some((1280, 720))));
        assert_eq!(scaled.frame_size(), (1280, 720), "the guest's window was ignored");

        assert!(scaled.set_size_granularity((64, 16)));
        let (w, h) = scaled.frame_size();
        assert!(w.is_multiple_of(64) && h.is_multiple_of(16), "off the grid at {w}x{h}");
        assert!(w <= 1280 && h <= 720, "the grid pushed it past the ceiling: {w}x{h}");

        // And a frame really comes out that size, rather than the size being a
        // claim the resampler does not honour.
        let frame = scaled.capture_next().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (w, h));
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
    use super::{fit_within, snap_to};

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

    /// The sizes measured against the real AV1 transform, as a rule.
    ///
    /// Every one of these was tried on the hardware: the left column stalled
    /// the encoder outright, and what `snap_to` turns it into did not.
    #[test]
    fn a_size_the_av1_encoder_refuses_is_snapped_to_one_it_takes() {
        for (bad, _why) in [
            ((1398u16, 874u16), "the session that reported this"),
            ((1392, 864), "width off the grid even with both axes on 16"),
            ((1632, 1024), "the same, one notch below a size that works"),
            ((2880, 1620), "width fine, height off 16"),
            ((1920, 1204), "the same, at the size a 2560x1600 primary lands on"),
            ((1636, 1022), "the wall"),
            ((1376, 816), "32-aligned is not 64-aligned"),
        ] {
            let (w, h) = snap_to(bad, (64, 16));
            assert!(w.is_multiple_of(64), "{bad:?} snapped to {w}x{h}, width off the grid");
            assert!(h.is_multiple_of(16), "{bad:?} snapped to {w}x{h}, height off the grid");
            assert!(w <= bad.0 && h <= bad.1, "{bad:?} grew to {w}x{h}");
        }

        // Sizes the encoder already takes are left exactly alone, so this never
        // costs a machine that did not need it anything.
        for good in [(1920u16, 1200u16), (2880, 1616), (1280, 720), (2560, 1600), (1216, 688)] {
            assert_eq!(snap_to(good, (64, 16)), good);
        }
    }

    /// The whole reason the height is derived rather than rounded on its own.
    #[test]
    fn snapping_keeps_the_shape_it_was_given() {
        for (w, h) in [(1382u16, 864u16), (1398, 874), (2456, 1382), (1636, 1022), (999, 733)] {
            let before = f32::from(w) / f32::from(h);
            let (sw, sh) = snap_to((w, h), (64, 16));
            let after = f32::from(sw) / f32::from(sh);
            let drift = (after - before).abs() / before;
            assert!(
                drift < 0.02,
                "{w}x{h} became {sw}x{sh}: aspect moved {:.1}%, which is visible",
                drift * 100.0
            );
        }
    }

    /// The default asks for nothing, and must therefore change nothing.
    #[test]
    fn the_ordinary_granularity_is_the_identity_on_an_even_size() {
        for size in [(1920u16, 1200u16), (1398, 874), (640, 480), (2, 2)] {
            assert_eq!(snap_to(size, (2, 2)), size);
        }
    }

    /// A stream degraded to almost nothing must still be a picture.
    #[test]
    fn snapping_never_reaches_zero() {
        for size in [(1u16, 1u16), (2, 2), (63, 15), (64, 16), (10, 4000)] {
            let (w, h) = snap_to(size, (64, 16));
            assert!(w >= 64 && h >= 16, "{size:?} snapped to {w}x{h}");
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
    #[cfg(target_os = "windows")]
    use super::is_virtual_adapter;
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

    /// Known virtual display drivers are recognised; real GPUs are not.
    ///
    /// The false positive is the one that matters. Labelling a real monitor
    /// "virtual" tells a guest that nobody is looking at a screen somebody is
    /// looking at, and invites resizing it under them. Missing a virtual one
    /// only costs a label.
    #[test]
    #[cfg(target_os = "windows")]
    fn virtual_adapters_are_told_apart_from_real_ones() {
        fn as_desc(name: &str) -> [u16; 128] {
            let mut buf = [0u16; 128];
            for (slot, unit) in buf.iter_mut().zip(name.encode_utf16()) {
                *slot = unit;
            }
            buf
        }

        for name in [
            "IddSampleDriver Device",
            "Virtual Display Driver",
            "MttVDD",
            "Parsec Virtual Display Adapter",
            "usbmmidd Device",
            // Case is not the driver's to choose.
            "VIRTUAL DISPLAY DRIVER",
        ] {
            assert!(is_virtual_adapter(&as_desc(name)), "{name} should read as virtual");
        }

        for name in [
            "AMD Radeon(TM) 8060S Graphics",
            "NVIDIA GeForce RTX 4090",
            "Intel(R) UHD Graphics 770",
            // WARP. Software, but not a virtual *display* — it has no outputs
            // and capture reaches it by a different route entirely.
            "Microsoft Basic Render Driver",
        ] {
            assert!(!is_virtual_adapter(&as_desc(name)), "{name} should not read as virtual");
        }
    }

    /// This machine's own displays are described consistently.
    ///
    /// Not an assertion about the hardware — a machine with a virtual display
    /// driver installed is a legitimate configuration and this must not fail on
    /// one. What it checks is that the flag and the enumeration agree.
    #[test]
    fn whether_a_display_is_virtual_is_reported_for_every_one() {
        for screen in displays() {
            // Nothing to assert about the value; what matters is that asking
            // does not panic and every display carries an answer.
            eprintln!(
                "{} — {}",
                screen.name,
                if screen.virtual_display { "virtual" } else { "real" }
            );
        }
    }
}

#[cfg(test)]
mod remedy_tests {
    use super::Remedy;
    use crate::i18n;
    use crate::protocol::end_reason::wrong_gpu;

    const ALL: [&str; 1] = [wrong_gpu::NEITHER_WAY];

    /// A remedy with no words is a code shown to a person.
    #[test]
    fn every_wrong_gpu_ending_has_a_sentence_in_every_catalogue() {
        for code in ALL {
            let key = Remedy::new(code, &[]).key();
            for language in i18n::languages() {
                let said = language.get(&key);
                assert!(said.is_some(), "{} has nothing for {key}", language.tag);
                assert!(
                    said.is_some_and(|text| text.contains("{screen}")),
                    "{} loses the display name from {key}",
                    language.tag
                );
            }
        }
    }

    /// The display is an identifier and travels as itself; the words around it
    /// do not.
    #[test]
    fn the_display_a_remedy_is_about_reaches_the_sentence() {
        let remedy = Remedy::new(wrong_gpu::NEITHER_WAY, &[("screen", r"\\.\DISPLAY2")]);
        assert!(remedy.to_string().contains(r"\\.\DISPLAY2"), "{remedy}");
    }

    /// The log stays English however the interface is set.
    ///
    /// Asserted without switching the active language, which is process-wide
    /// and would race every other test in this binary: what makes the claim
    /// real is that `Display` reads the English catalogue directly, and that
    /// the Russian one genuinely says something else — so the two cannot be
    /// passing by being identical.
    #[test]
    fn a_remedy_speaks_english_to_the_log_whatever_the_screen_shows() {
        let key = Remedy::new(wrong_gpu::NEITHER_WAY, &[]).key();
        let english = i18n::languages().iter().find(|l| l.tag == "en").expect("English");
        let russian = i18n::languages().iter().find(|l| l.tag == "ru").expect("Russian");

        assert_eq!(i18n::in_english(&key), english.get(&key).expect("English has it"));
        assert_ne!(
            english.get(&key),
            russian.get(&key),
            "{key} reads the same in both, so this proves nothing"
        );
    }

    /// Each ending says its own thing.
    ///
    /// There were five, one per branch of an automatic fix that pinned Tain to
    /// the integrated chip. The fix is gone — it bought duplication at the cost
    /// of the encoder — and with it four of the endings. This guards the shape
    /// rather than the count: whatever is here, no two of them may be the same
    /// sentence, or one of them is not worth sending.
    #[test]
    fn no_two_wrong_gpu_endings_say_the_same_thing() {
        let mut seen = std::collections::BTreeSet::new();
        for code in ALL {
            let key = Remedy::new(code, &[]).key();
            assert!(seen.insert(i18n::in_english(&key)), "{code} repeats another ending");
        }
    }

    /// Nothing in the capture path writes a graphics preference any more.
    ///
    /// # What this is guarding
    ///
    /// The old code wrote one as a *side effect* of duplication failing —
    /// before the compositor had even been tried, and on a path nobody was
    /// looking at. It worked, capture started, and the cost only showed up
    /// later as an encoder that would not use the discrete card. A grep is a
    /// crude test and it is the right shape for this: the fault was that a
    /// registry write existed somewhere it had no business being.
    #[test]
    fn the_capture_path_does_not_change_this_machines_graphics_settings() {
        let capture = include_str!("capture_dxgi.rs");
        assert!(
            !capture.contains("gpu_preference::set"),
            "the capture path is writing a graphics preference again — that is what \
             pinned Tain to the integrated chip and kept the encoder off the discrete one"
        );
        let shared = include_str!("capture_shared.rs");
        assert!(!shared.contains("gpu_preference::set"), "and neither should this one");
    }
}
