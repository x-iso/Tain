//! Real screen capture on Windows via DXGI Desktop Duplication.
//!
//! Chosen over `scap` (dependency conflicts) and `windows-capture` (another
//! crate) because the `windows` crate is already a dependency for input
//! injection, and Desktop Duplication is what most remote desktop software uses:
//! the compositor hands over the already-composed desktop surface, so there is
//! no per-window enumeration and no redraw cost.
//!
//! ## Lifetime hazards this handles
//!
//! Duplication is *lost*, not merely interrupted, on a surprising number of
//! events: resolution changes, GPU driver restarts, a UAC prompt (which runs on
//! a separate secure desktop), fast user switching, and locking the screen. All
//! of these surface as `DXGI_ERROR_ACCESS_LOST`, and the only fix is to rebuild
//! the whole duplication chain. [`DxgiCapturer`] does that transparently.

use anyhow::{Context, Result};
use bytes::Bytes;
use tracing::*;

use windows::core::Interface;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

use super::capture_hdr::{ColourLog, DisplayColour, HdrToSdr};
use super::capture_nv12::Nv12Converter;
use super::screen_capture::{PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame};

/// How long to block waiting for the desktop to change, per capture call.
///
/// Short: returning `None` promptly lets the session loop stay responsive on a
/// completely static desktop rather than stalling its frame ticker.
///
/// It is also why the pipeline must not treat this call's duration as "capture
/// cost" — waiting for something to happen is not the same as being unable to
/// keep up, and counting it made an idle desktop report itself as slow capture.
const ACQUIRE_TIMEOUT_MS: u32 = 16;

/// How often an unchanged desktop is re-sent anyway.
///
/// Only reason it happens at all: a guest connecting to an idle machine needs
/// *a* picture, and the screen may not change for minutes. One frame a second
/// costs nothing and bounds how long a new guest stares at black.
const REFRESH_IDLE: std::time::Duration = std::time::Duration::from_millis(1000);

/// Everything that has to be rebuilt when duplication is lost.
///
/// The device must outlive everything here — the duplication, the staging
/// texture and the converter all belong to it — and is also what the GPU colour
/// conversion is built from, so it is kept rather than dropped.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutputDuplication,
    /// CPU-readable texture the GPU frame is copied into.
    ///
    /// Only used on the BGRA path. When `converter` is present the frame goes
    /// through the video processor instead and this stays untouched.
    staging: ID3D11Texture2D,
    /// GPU colour conversion, when it was asked for and the driver has it.
    converter: Option<Nv12Converter>,
    /// Brings a floating-point HDR desktop back to BGRA. `None` on an ordinary
    /// SDR display, which is the usual case and needs nothing.
    hdr: Option<HdrToSdr>,
    width: u16,
    height: u16,
    /// Pointer state since it was last taken. See [`Duplication::read_pointer`].
    cursor_position: Option<super::cursor::CursorPosition>,
    cursor_shape: Option<super::cursor::CursorShape>,
    /// Reused across shape reads, so a pointer crossing windows does not
    /// allocate on every hop.
    pointer_buffer: Vec<u8>,
}

impl Duplication {
    /// # Why this takes something to log through
    ///
    /// Everything here is rebuilt on `DXGI_ERROR_ACCESS_LOST`, and when the
    /// rebuild cannot succeed — a UAC prompt, or an HDR desktop this could not
    /// duplicate — it is attempted again on the next capture call, about
    /// twenty-five times a second. Anything said unconditionally in here is
    /// therefore said twenty-five times a second: the log from the machine that
    /// found the HDR fault is two lines repeated for its whole length, which is
    /// most of what made it hard to read.
    ///
    /// The state that decides what is worth repeating has to outlive the thing
    /// being rebuilt, so it is passed in.
    fn create(colour_log: &mut ColourLog) -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        // Try the real GPU first; WARP keeps capture working in VMs and on
        // machines whose driver refuses hardware duplication.
        let mut last_err = None;
        for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            // SAFETY: all out-params are valid; a failed call leaves them None.
            let result = unsafe {
                D3D11CreateDevice(
                    None,
                    driver,
                    None,
                    // VIDEO_SUPPORT is what makes `ID3D11VideoDevice` available,
                    // and with it the GPU colour conversion. It costs nothing on
                    // a driver that has it and is why the loop below still tries
                    // WARP: a device without it simply keeps the CPU path.
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };
            match result {
                Ok(()) => break,
                Err(e) => last_err = Some(e),
            }
        }

        let device = device
            .ok_or_else(|| anyhow::anyhow!("Could not create a D3D11 device: {last_err:?}"))?;
        let context = context.context("D3D11 device came back without a context")?;

        // Capture runs on its own thread and the video processor and the encoder
        // may touch this device from others. Without this the driver is free to
        // assume single-threaded access, and the failure is a corrupt frame or a
        // device removal rather than an error anyone can trace back here.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            // SAFETY: the device is live; this only sets a flag on it.
            unsafe { let _ = mt.SetMultithreadProtected(true); };
        }

        // SAFETY: each cast/query is checked; the COM objects outlive the calls.
        let (output, width, height, hdr) = unsafe {
            let dxgi_device: IDXGIDevice = device.cast().context("device is not a IDXGIDevice")?;
            let adapter = dxgi_device.GetAdapter().context("no DXGI adapter")?;
            let output = adapter.EnumOutputs(0).context("no display attached to adapter 0")?;
            let output1: IDXGIOutput1 = output.cast().context("output is not IDXGIOutput1")?;

            let colour = super::capture_hdr::probe(&output);
            colour_log.display(colour);
            let hdr_desktop = colour.is_some_and(DisplayColour::is_hdr);

            let mut duplication = duplicate(&device, &output1, hdr_desktop)?;
            // Take the size from the duplication itself rather than the output:
            // it is what we actually capture, and it avoids pulling in the GDI
            // feature that `IDXGIOutput::GetDesc` needs for its HMONITOR field.
            let mut size = duplication_size(&duplication);

            // # Why the converter is built before the first frame
            //
            // The floating-point desktop is only worth asking for if there is
            // something able to convert it. Discovering otherwise once frames
            // are arriving would leave every path below holding a surface in a
            // format it cannot read, and the recovery for that — tear the
            // duplication down and build it again — is the same thing this does
            // here, once, before anything depends on it.
            //
            // It needs the size, and the size comes from the duplication, which
            // is why this is not simply asked first.
            let mut converter = None;
            if hdr_desktop {
                // What the display says white is worth, or 80 nits if it will
                // not say — see `DisplayColour::reference_white`. Getting this
                // wrong does not fail, it just washes the picture out.
                let white = colour.map_or(1.0, |colour| colour.reference_white);
                match HdrToSdr::new(&device, &context, size.0, size.1, white) {
                    Ok(ready) => converter = Some(ready),
                    Err(e) => {
                        colour_log.conversion_refused(&e);
                        // # The first duplication has to be let go of first
                        //
                        // DXGI permits one duplication per output per process,
                        // and asking for a second while the first is alive is
                        // refused. `duplication = duplicate(..)` evaluates the
                        // new one before dropping the old, so the fallback
                        // added to rescue an HDR desktop was itself asking for
                        // something impossible — and answered, as the machine
                        // that reported it showed, with the same "parameter is
                        // incorrect" as the call it was replacing.
                        drop(duplication);
                        duplication = duplicate(&device, &output1, false)?;
                        size = duplication_size(&duplication);
                    }
                }
            }

            (duplication, size.0, size.1, converter)
        };

        let staging = create_staging_texture(&device, width, height)?;

        info!("DXGI Desktop Duplication ready — {width}x{height}");
        Ok(Self {
            device,
            context,
            output,
            staging,
            hdr,
            converter: None,
            width,
            height,
            cursor_position: None,
            cursor_shape: None,
            pointer_buffer: Vec::new(),
        })
    }

    /// Grab one frame, or `None` if the desktop has not changed.
    ///
    /// Returns the pixels and **how long the work took**, which is not the same
    /// as how long this call took: most of a call on an idle desktop is spent
    /// blocked in `AcquireNextFrame` waiting for something to move.
    fn next_frame(&mut self) -> Result<Option<(Vec<u8>, std::time::Duration)>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: out-params are valid; every path releases the frame below.
        let acquired =
            unsafe { self.output.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource) };

        if let Err(e) = acquired {
            return match e.code() {
                // Nothing on screen changed. Not an error.
                code if code == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                code if code == DXGI_ERROR_ACCESS_LOST => {
                    anyhow::bail!("access lost")
                }
                _ => Err(anyhow::anyhow!("AcquireNextFrame failed: {e}")),
            };
        }

        // A pointer update arrives on the same wake-up, and often *instead* of
        // a desktop change: moving the mouse over a still screen produces a
        // frame with nothing new in it but a new cursor position. Reading it
        // here is what makes the pointer move at all.
        self.read_pointer(&info);

        // A zero present time means this wake-up was a *pointer* update — the
        // cursor moved but the desktop image did not, and the texture handed
        // back carries no new content. Copying it yields a blank frame.
        let desktop_changed = info.LastPresentTime != 0;

        // Always release, even if the copy below fails — holding a frame blocks
        // every subsequent acquire. The clock starts *here*, after the wait.
        let started = std::time::Instant::now();
        let result = if desktop_changed { self.copy_acquired(resource) } else { Ok(None) };
        let result = result.map(|pixels| pixels.map(|p| (p, started.elapsed())));
        // SAFETY: paired with the successful AcquireNextFrame above.
        let _ = unsafe { self.output.ReleaseFrame() };
        result
    }

    /// Take whatever the last acquire said about the pointer.
    ///
    /// Position is reported only when it changed; shape only when *that*
    /// changed, which is when the pointer crosses a text field or a resize
    /// handle rather than per frame.
    fn read_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        // `LastMouseUpdateTime` of zero means nothing about the pointer changed
        // on this acquire, and `PointerPosition` is stale rather than current.
        if info.LastMouseUpdateTime != 0 {
            let position = info.PointerPosition;
            self.cursor_position = Some(super::cursor::CursorPosition {
                x: position.Position.x,
                y: position.Position.y,
                // The duplication's own size, which is the desktop — not
                // whatever the stream has been scaled to.
                desktop_width: self.width,
                desktop_height: self.height,
                visible: position.Visible.as_bool(),
            });
        }

        // Non-zero only when the driver has a new image for us.
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        let size = info.PointerShapeBufferSize as usize;
        self.pointer_buffer.resize(size.max(self.pointer_buffer.len()), 0);
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required = 0u32;

        // SAFETY: the buffer is at least `size` bytes and its length is passed;
        // both out-params are valid.
        let got = unsafe {
            self.output.GetFramePointerShape(
                self.pointer_buffer.len() as u32,
                self.pointer_buffer.as_mut_ptr() as *mut _,
                &mut required,
                &mut shape_info,
            )
        };
        if got.is_err() {
            return;
        }

        let Some(kind) = super::cursor::ShapeKind::from_raw(shape_info.Type) else {
            // An unrecognised type is left alone rather than guessed at: drawing
            // one with the wrong interpretation is a block of noise on screen.
            debug!("Ignoring pointer shape of unknown type {}", shape_info.Type);
            return;
        };
        let Some((rgba, height)) = super::cursor::to_rgba(
            kind,
            &self.pointer_buffer[..required as usize],
            shape_info.Width,
            shape_info.Height,
            shape_info.Pitch,
        ) else {
            return;
        };

        // Content-addressed, so a pointer the guest has already cached is never
        // sent twice — Windows re-reports the same handful of shapes constantly
        // as the mouse moves between windows.
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hash::hash(&rgba, &mut hasher);
        let id = std::hash::Hasher::finish(&hasher);

        self.cursor_shape = Some(super::cursor::CursorShape {
            id,
            width: shape_info.Width as u16,
            height: height as u16,
            hotspot_x: shape_info.HotSpot.x as u16,
            hotspot_y: shape_info.HotSpot.y as u16,
            rgba,
        });
    }

    /// Pointer state since the last call, if anything changed.
    pub fn take_cursor(&mut self) -> Option<super::cursor::CursorUpdate> {
        let position = self.cursor_position.take()?;
        Some(super::cursor::CursorUpdate { position, shape: self.cursor_shape.take() })
    }

    /// Turn on GPU colour conversion, if this driver can do it.
    ///
    /// Returns whether it took. Failure is not an error the caller has to
    /// handle — the BGRA path is still there and still correct.
    fn enable_nv12(&mut self, out_width: u16, out_height: u16) -> bool {
        match Nv12Converter::scaling(
            &self.device,
            &self.context,
            self.width,
            self.height,
            out_width,
            out_height,
        ) {
            Ok(converter) => {
                self.converter = Some(converter);
                true
            }
            Err(e) => {
                info!("Staying on CPU colour conversion: {e:#}");
                self.converter = None;
                false
            }
        }
    }

    fn copy_acquired(&mut self, resource: Option<IDXGIResource>) -> Result<Option<Vec<u8>>> {
        let Some(resource) = resource else { return Ok(None) };
        let texture: ID3D11Texture2D =
            resource.cast().context("duplicated resource is not a texture")?;

        // An HDR desktop arrives in floating point and has to be brought back
        // into the range every path below this assumes. Decided from the format
        // that actually turned up rather than from what the display said,
        // because the surface is the thing that has to match.
        let texture = match self.hdr.as_mut() {
            Some(hdr) if is_floating_point(&texture) => hdr.convert(&texture)?,
            _ => texture,
        };

        if let Some(converter) = self.converter.as_mut() {
            // The whole point: the frame is converted and subsampled on the GPU,
            // so what crosses PCIe is 12 bits per pixel instead of 32 and the
            // encoder gets the layout it wanted without the CPU touching it.
            return converter.convert(&texture).map(Some);
        }

        // SAFETY: staging matches the source format and size; unmapped below.
        unsafe {
            self.context.CopyResource(&self.staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not map the staging texture")?;

            let pixels = read_mapped(&mapped, self.width, self.height);
            self.context.Unmap(&self.staging, 0);
            Ok(Some(pixels))
        }
    }
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

/// Start duplication, in the best format this desktop and this GPU allow.
///
/// # Why the legacy call is the last resort and not the fallback
///
/// It was the fallback, and on an HDR desktop that is a dead end:
/// `IDXGIOutput1::DuplicateOutput` cannot duplicate one at all. It answers
/// `E_INVALIDARG` — measured on the machine that reported this, where turning
/// HDR on froze the picture and then refused every reconnection while it stayed
/// on. Capture never came back, because the thing being retried could not work.
///
/// `DuplicateOutput1` is the only call that can be asked for a format, and that
/// is what makes it the fallback as well as the preferred path. Asked for
/// eight-bit on an HDR desktop it converts, on its own terms and probably
/// washed out — but it produces frames, which the legacy call does not.
///
/// It is also documented to fail with `E_INVALIDARG` for a process that is not
/// DPI aware, which is true and was measured. Every process that reaches here
/// has declared itself — the application through winit's event loop, the agent
/// through the call at the top of `main` — but neither is free to stop, and
/// this is what would break if one did.
///
/// So: floating point if there is something able to convert it, eight-bit
/// through the same call if not, and the legacy call only for a Windows too old
/// to offer `IDXGIOutput5` — which is also a Windows too old to have an HDR
/// desktop, so it is never the one being asked to do the impossible.
fn duplicate(
    device: &ID3D11Device,
    output1: &IDXGIOutput1,
    floating_point: bool,
) -> Result<IDXGIOutputDuplication> {
    let denied = |e: &windows::core::Error| e.code() == E_ACCESSDENIED;
    let explain = |e: windows::core::Error| {
        if denied(&e) {
            anyhow::anyhow!(
                "Screen capture was denied. This happens on the lock screen or while \
                 a UAC prompt is up — both run on a desktop we cannot see."
            )
        } else {
            anyhow::anyhow!("DuplicateOutput failed: {e}")
        }
    };

    if let Ok(output5) = output1.cast::<IDXGIOutput5>() {
        let mut wanted: Vec<(&str, DXGI_FORMAT)> = Vec::new();
        if floating_point {
            wanted.push(("floating point", DXGI_FORMAT_R16G16B16A16_FLOAT));
        }
        wanted.push(("eight-bit", DXGI_FORMAT_B8G8R8A8_UNORM));

        for (label, format) in wanted {
            // SAFETY: the format list outlives the call; the device is live.
            match unsafe { output5.DuplicateOutput1(device, 0, &[format]) } {
                Ok(duplication) => {
                    debug!("Duplicating the desktop in {label}");
                    return Ok(duplication);
                }
                // Denial is about the desktop, not the format. Asking again in
                // another format would be denied in exactly the same way, and
                // the message for it is the useful one.
                Err(e) if denied(&e) => return Err(explain(e)),
                Err(e) => debug!("DuplicateOutput1 would not give {label}: {e}"),
            }
        }
    }

    // SAFETY: the device outlives the duplication it is asked for.
    unsafe { output1.DuplicateOutput(device) }.map_err(explain)
}

/// The size duplication is actually handing over, clamped to what the wire
/// carries.
fn duplication_size(duplication: &IDXGIOutputDuplication) -> (u16, u16) {
    // SAFETY: reads a descriptor from a live duplication.
    let mode = unsafe { duplication.GetDesc() }.ModeDesc;
    (
        mode.Width.clamp(1, u16::MAX as u32) as u16,
        mode.Height.clamp(1, u16::MAX as u32) as u16,
    )
}

/// Whether an acquired surface is the floating-point kind an HDR desktop
/// produces.
///
/// Asked of the surface rather than of the display, because the surface is what
/// every stage below has to be able to read. If the two ever disagree — a
/// driver ignoring the requested format, HDR switched on between one frame and
/// the next — this is the one that is right.
fn is_floating_point(texture: &ID3D11Texture2D) -> bool {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: a documented read into a local, on a live texture.
    unsafe { texture.GetDesc(&mut desc) };
    desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT
}

fn create_staging_texture(
    device: &ID3D11Device,
    width: u16,
    height: u16,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width as u32,
        Height: height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        // Staging + CPU read is the only combination that can be mapped for
        // reading; the GPU copy above lands here.
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };

    let mut texture = None;
    // SAFETY: desc is fully initialised; the out-param is checked below.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("could not create the staging texture")?;
    texture.context("CreateTexture2D returned nothing")
}

/// Captures the primary display through DXGI Desktop Duplication.
pub struct DxgiCapturer {
    duplication: Option<Duplication>,
    /// Last full frame, for the periodic refresh described on [`REFRESH_IDLE`].
    ///
    /// `Bytes` rather than `Vec<u8>`: this is 16 MB at 2560x1600 and it used to
    /// be cloned on every single tick, which is a memcpy of the whole frame for
    /// nothing. Cloning `Bytes` bumps a refcount.
    last_frame: Option<Bytes>,
    /// When the last frame was actually handed out, for the idle refresh.
    last_emitted: Option<std::time::Instant>,
    width: u16,
    height: u16,
    /// Size the frames are emitted at, which is the display's unless the
    /// controller has asked for less.
    ///
    /// On the NV12 path the rescale happens inside the video processor, in the
    /// same pass as the colour conversion and at no measurable cost. There is
    /// deliberately no CPU fallback for scaling here: `Scaled` still wraps the
    /// backends that need it, and doing it twice would be worse than either.
    out_width: u16,
    out_height: u16,
    /// The quality scale the controller last asked for.
    scale: QualityScale,
    /// The guest's viewport, if it has told us. Composed with `scale` rather
    /// than replacing it: the controller sheds pixels under load, and the guest
    /// caps what is worth sending at all.
    ceiling: Option<(u16, u16)>,
    /// What the frames coming out of here are laid out as.
    ///
    /// Negotiated once before the session starts and then fixed. It survives a
    /// rebuild after `DXGI_ERROR_ACCESS_LOST` deliberately: the format is a
    /// contract with the encoder, and quietly reverting to BGRA halfway through
    /// a session would feed it bytes it would read as NV12.
    format: PixelFormat,
    /// Suppresses a log storm while duplication stays unavailable.
    reported_loss: bool,
    /// What has already been said about the display's colour handling.
    ///
    /// Outlives the duplication it describes, because the duplication is what
    /// keeps being rebuilt — see [`ColourLog`].
    colour_log: ColourLog,
}

impl DxgiCapturer {
    pub fn new() -> Result<Self> {
        let mut colour_log = ColourLog::default();
        let duplication = Duplication::create(&mut colour_log)?;
        let (width, height) = (duplication.width, duplication.height);
        Ok(Self {
            duplication: Some(duplication),
            last_frame: None,
            last_emitted: None,
            width,
            height,
            out_width: width,
            out_height: height,
            scale: QualityScale::Native,
            ceiling: None,
            format: PixelFormat::Bgra,
            reported_loss: false,
            colour_log,
        })
    }

    /// What the frames coming out of here are laid out as.
    ///
    /// Asked by [`super::capture_shared`], where a second session needs to know
    /// whether the format it wants is already the one being produced — the
    /// answer decides whether it can join or has to take what is on offer.
    pub fn current_format(&self) -> PixelFormat {
        self.format
    }

    /// Rebuild after `DXGI_ERROR_ACCESS_LOST`.
    ///
    /// Failure here is expected and temporary — the lock screen and UAC prompts
    /// both deny duplication for as long as they are up.
    fn recover(&mut self) {
        self.duplication = None;
        match Duplication::create(&mut self.colour_log) {
            Ok(duplication) => {
                if duplication.width != self.width || duplication.height != self.height {
                    info!(
                        "Display changed from {}x{} to {}x{}",
                        self.width, self.height, duplication.width, duplication.height
                    );
                    self.width = duplication.width;
                    self.height = duplication.height;
                    self.last_frame = None;
                }
                self.duplication = Some(duplication);
                if self.format == PixelFormat::Nv12
                    && !self
                        .duplication
                        .as_mut()
                        .unwrap()
                        .enable_nv12(self.out_width, self.out_height)
                {
                    // This used to stop capture outright, on the grounds that
                    // the encoder could not be told mid-session and a stream of
                    // misread bytes is worse than no stream.
                    //
                    // The encoder does not need telling. Each frame carries its
                    // own format and the encoder reads it there, so dropping
                    // back to BGRA costs the CPU conversion and nothing else —
                    // which is plainly better than a black screen for the rest
                    // of the session. The buffered frame goes because it is
                    // half of the old arrangement.
                    warn!(
                        "GPU colour conversion did not survive the rebuild — \
                         converting on the CPU instead"
                    );
                    self.format = PixelFormat::Bgra;
                    self.last_frame = None;
                }
                self.reported_loss = false;
            }
            Err(e) => {
                if !self.reported_loss {
                    warn!("Screen capture unavailable, retrying: {e}");
                    self.reported_loss = true;
                }
            }
        }
    }
}

impl DxgiCapturer {
    /// Work out the emitted size from the scale and the guest's ceiling, and
    /// reconfigure whatever produces it.
    ///
    /// Both inputs are applied here rather than at their call sites so there is
    /// one place that decides, and it cannot end up half-applied — a stale
    /// converter producing a different size from the one the frames claim is a
    /// picture torn into diagonal stripes, not an error.
    fn apply_output_size(&mut self) -> bool {
        let scaled = self.scale.apply((self.width, self.height));
        let (w, h) = match self.ceiling {
            Some(ceiling) => super::screen_capture::fit_within(scaled, ceiling),
            None => (scaled.0.max(2) & !1, scaled.1.max(2) & !1),
        };
        if (w, h) == (self.out_width, self.out_height) {
            return true;
        }

        if self.format == PixelFormat::Nv12 {
            let Some(duplication) = self.duplication.as_mut() else { return false };
            if !duplication.enable_nv12(w, h) {
                // The converter is gone and nothing else here produces NV12.
                // Keep the size we had rather than emitting the wrong format.
                return false;
            }
            info!("Capture now {w}x{h} on the GPU ({})", self.scale.label());
        } else {
            info!("Capture now {w}x{h} on the CPU ({})", self.scale.label());
        }

        self.out_width = w;
        self.out_height = h;
        self.last_frame = None;
        true
    }

    /// Wrap the last frame for another send, if the idle refresh is due.
    ///
    /// **An unchanged desktop must not cost anything.** This used to return the
    /// last frame on *every* tick, so a session watching a still screen re-cloned
    /// 16 MB, re-encoded it and re-sent it sixty times a second — burning the
    /// whole bitrate to transmit nothing. On a link that could not carry that,
    /// the frame queue filled, the capture thread blocked, and the picture
    /// froze: reported as "slow network", caused by an idle screen.
    ///
    /// A slow refresh still happens, because a guest that connects mid-idle
    /// needs *a* picture and the desktop may not change for minutes.
    fn idle_refresh(&mut self) -> Option<ScreenFrame> {
        let due = self
            .last_emitted
            .is_none_or(|at| at.elapsed() >= REFRESH_IDLE);
        if !due {
            return None;
        }
        let data = self.last_frame.clone()?;
        self.last_emitted = Some(std::time::Instant::now());
        // Nothing was produced — this is the frame we already had.
        Some(ScreenFrame::with_format(
            self.out_width,
            self.out_height,
            data,
            self.format,
            std::time::Duration::ZERO,
        ))
    }
}

impl ScreenCaptureBackend for DxgiCapturer {
    /// Pointer state, when the duplication is live.
    ///
    /// Nothing during a recovery — a UAC prompt tears the duplication down and
    /// the pointer is not ours to report until it comes back.
    fn take_cursor(&mut self) -> Option<super::cursor::CursorUpdate> {
        self.duplication.as_mut()?.take_cursor()
    }

    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let Some(duplication) = self.duplication.as_mut() else {
            self.recover();
            // Keep the stream alive rather than stalling the session while a UAC
            // prompt is up — but at the idle rate, not the full one.
            return Ok(self.idle_refresh());
        };

        match duplication.next_frame() {
            Ok(Some((pixels, cost))) => {
                // On the NV12 path the video processor already produced this at
                // the emitted size; on the BGRA path it is still full size.
                let (pixels, cost) = if self.format == PixelFormat::Bgra
                    && (self.out_width, self.out_height) != (self.width, self.height)
                {
                    let started = std::time::Instant::now();
                    let full = ScreenFrame::new(
                        self.width,
                        self.height,
                        Bytes::from(pixels),
                        cost,
                    );
                    let scaled = super::screen_capture::box_downscale(
                        &full,
                        self.out_width,
                        self.out_height,
                    );
                    (scaled, cost + started.elapsed())
                } else {
                    (pixels, cost)
                };
                let data = Bytes::from(pixels);
                self.last_frame = Some(data.clone());
                self.last_emitted = Some(std::time::Instant::now());
                Ok(Some(ScreenFrame::with_format(
                    self.out_width,
                    self.out_height,
                    data,
                    self.format,
                    cost,
                )))
            }
            // Nothing changed. Send nothing, which is what an idle screen costs.
            Ok(None) => Ok(self.idle_refresh()),
            Err(_) => {
                debug!("Duplication lost — rebuilding");
                self.recover();
                Ok(None)
            }
        }
    }

    fn frame_size(&self) -> (u16, u16) {
        (self.out_width, self.out_height)
    }

    fn request_format(&mut self, format: PixelFormat) -> bool {
        if format == self.format {
            return true;
        }
        match format {
            // Nothing asks to go back, and there is no reason to support it:
            // the encoder is built around whatever was agreed at the start.
            PixelFormat::Bgra => false,
            PixelFormat::Nv12 => {
                let Some(duplication) = self.duplication.as_mut() else { return false };
                if !duplication.enable_nv12(self.out_width, self.out_height) {
                    return false;
                }
                self.format = PixelFormat::Nv12;
                // The buffered frame is BGRA and would be read as NV12.
                self.last_frame = None;
                true
            }
        }
    }

    /// Rescale, on the GPU when the frame is already going through the video
    /// processor and on the CPU when it is not.
    ///
    /// Owned here rather than left to the `Scaled` wrapper because the two
    /// decisions are entangled: GPU scaling rides on the NV12 converter, and
    /// which format is in use is not settled until the encoder has been asked.
    /// A wrapper chosen before that question is answered can only ever guess,
    /// and it guessed wrong — every session that dropped below full resolution
    /// silently lost the GPU conversion and paid `box_downscale` instead.
    fn set_scale(&mut self, scale: QualityScale) -> bool {
        self.scale = scale;
        self.apply_output_size()
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        if self.ceiling == ceiling {
            return true;
        }
        self.ceiling = ceiling;
        self.apply_output_size()
    }

    fn is_real(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::cursor as tain_core_cursor;

    /// The real pointer, from the real desktop.
    ///
    /// Move the mouse while this runs. It asserts only what must be true —
    /// that positions arrive and any shape decodes to a sane image — because
    /// where the pointer *is* depends on where the user left it.
    #[test]
    #[ignore = "captures the real desktop; move the mouse while it runs"]
    fn the_pointer_is_reported_with_a_usable_shape() {
        let Ok(mut capturer) = DxgiCapturer::new() else {
            eprintln!("no desktop duplication — skipping");
            return;
        };
        let (screen_w, screen_h) = capturer.frame_size();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut positions = 0;
        let mut shapes = 0;
        let mut last: Option<tain_core_cursor::CursorPosition> = None;
        while std::time::Instant::now() < deadline {
            let _ = capturer.capture_next();
            let Some(update) = capturer.take_cursor() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            positions += 1;
            last = Some(update.position);

            if let Some(shape) = update.shape {
                shapes += 1;
                // What the shape is actually made of. A pointer that renders as
                // a solid block means one state is swallowing the others, and
                // the distribution says which.
                let mut opaque = 0;
                let mut clear = 0;
                let mut white = 0;
                for px in shape.rgba.chunks_exact(4) {
                    if px[3] == 0 {
                        clear += 1;
                    } else {
                        opaque += 1;
                        if px[0] == 255 && px[1] == 255 && px[2] == 255 {
                            white += 1;
                        }
                    }
                }
                println!(
                    "  shape {}x{} hotspot {},{} — {opaque} opaque ({white} pure white), {clear} transparent",
                    shape.width, shape.height, shape.hotspot_x, shape.hotspot_y
                );
                assert!(shape.width > 0 && shape.height > 0, "shape has no size");
                assert_eq!(
                    shape.rgba.len(),
                    shape.width as usize * shape.height as usize * 4,
                    "shape buffer does not match its stated size"
                );
                assert!(
                    shape.hotspot_x <= shape.width && shape.hotspot_y <= shape.height,
                    "hotspot {}x{} is outside a {}x{} shape",
                    shape.hotspot_x,
                    shape.hotspot_y,
                    shape.width,
                    shape.height
                );
                // A pointer that is entirely transparent would draw as nothing,
                // which is indistinguishable from the feature not working.
                assert!(
                    shape.rgba.chunks(4).any(|px| px[3] > 0),
                    "decoded shape is fully transparent"
                );
            }
        }

        assert!(positions > 0, "no pointer updates in 2 seconds — is the mouse moving?");
        if let Some(p) = last {
            // Positions are in captured-frame pixels and must land on screen.
            assert!(
                p.x >= 0 && p.y >= 0 && p.x < screen_w as i32 && p.y < screen_h as i32,
                "pointer at {},{} is outside a {screen_w}x{screen_h} screen",
                p.x,
                p.y
            );
        }
        println!("{positions} position updates, {shapes} distinct shapes");
    }

    use super::*;

    /// Only meaningful on a machine with an interactive desktop; CI agents and
    /// locked sessions cannot duplicate an output at all.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn captures_a_real_frame_from_the_desktop() {
        let mut capturer = DxgiCapturer::new().expect("could not start duplication");
        let (w, h) = capturer.frame_size();
        assert!(w >= 640 && h >= 480, "implausible desktop size {w}x{h}");

        // A static desktop legitimately yields nothing, so keep asking for a
        // couple of seconds — something on screen will blink or repaint.
        let mut frame = None;
        for _ in 0..100 {
            if let Some(f) = capturer.capture_next().unwrap() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let frame = frame.expect("no frame after two seconds of trying");
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(frame.data.len(), w as usize * h as usize * 4);
        // A real desktop is never uniformly one colour.
        assert!(
            frame.data.chunks_exact(4).any(|px| px != &frame.data[0..4]),
            "captured frame is a single flat colour — probably not the real desktop"
        );
    }

    /// What actually happens when a second session asks to capture.
    ///
    /// Two guests at once, and a reconnect before the old session has let go,
    /// both landed on the test pattern instead of the desktop. This is the
    /// measurement that says why rather than guessing at the error.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn what_a_second_duplication_does() {
        let first = DxgiCapturer::new().expect("first duplication");
        println!("first: {:?}", first.frame_size());

        match DxgiCapturer::new() {
            Ok(second) => println!("second: ok, {:?}", second.frame_size()),
            Err(e) => println!("second: {e}"),
        }

        drop(first);
        match DxgiCapturer::new() {
            Ok(third) => println!("after dropping the first: ok, {:?}", third.frame_size()),
            Err(e) => println!("after dropping the first: {e}"),
        }
    }

    #[test]
    fn row_padding_is_stripped_when_copying() {
        // The driver pads rows; copying the mapping wholesale skews the image.
        let width = 3u16;
        let height = 2u16;
        let pitch = 16usize; // deliberately wider than 3 * 4 = 12
        let mut source = vec![0u8; pitch * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize * 4 {
                source[y * pitch + x] = (y * 100 + x) as u8;
            }
        }

        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: source.as_mut_ptr() as *mut _,
            RowPitch: pitch as u32,
            DepthPitch: 0,
        };
        // SAFETY: source outlives the call and is large enough.
        let packed = unsafe { read_mapped(&mapped, width, height) };

        assert_eq!(packed.len(), width as usize * height as usize * 4);
        assert_eq!(&packed[0..12], &source[0..12], "first row");
        assert_eq!(&packed[12..24], &source[pitch..pitch + 12], "second row must skip padding");
    }
}
