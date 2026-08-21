//! H.264 decoding via Media Foundation.
//!
//! The mirror of [`super::codec_mf`], and it exists for the same reason. A guest
//! decoding 2560x1600 in software spends tens of milliseconds per frame on the
//! CPU while its GPU sits idle — measured on a real two-machine session, that
//! cost plus the draw after it was the guest's dominant expense and surfaced to
//! the host as "slow decoding" and "slow display" simultaneously.
//!
//! ## This does not decode on the GPU, and it cannot be made to
//!
//! It was written believing it did. It does not. Measured at 1920x1080 on an
//! 18.3 Mbit/s stream by `decode_stage_cost` in this file:
//!
//! | | |
//! |---|---|
//! | `ProcessOutput` | **11.0 ms/frame** |
//! | OpenH264, unambiguously software | 14.7 ms/frame |
//! | GPU video codec engine, during the run | **1.6%** |
//! | GPU video processor (the colour convert) | 12.7% |
//! | Process CPU | **one core, pegged** |
//!
//! Being 1.34x faster than a known software decoder is not what hardware looks
//! like. Hardware decode of 1080p is under a millisecond.
//!
//! Everything the documentation asks for is in place: the transform reports
//! `MF_SA_D3D11_AWARE = 1`, it is sent `MFT_MESSAGE_SET_D3D_MANAGER` before any
//! type is negotiated, and the GPU advertises `H264_VLD_NOFGT` with NV12 output
//! (`decoder_profiles` lists 20 profiles). It accepts all of it, hands back
//! D3D11-backed output samples — which is what made this look like it was
//! working — and decodes into them on the CPU.
//!
//! Tried and disproven, so nobody repeats them:
//!
//! - **Output pool depth.** `MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT` swept 1, 4, 6,
//!   16: 11.3, 11.5, 11.9, 11.5 ms. Flat.
//! - **Frame size on the input type**, so the transform could ask the driver
//!   what it supports: 11.07 ms with, 10.92 ms without.
//! - **Draining harder.** The second `ProcessOutput` per frame costs 0.19 ms.
//! - **The readback.** Two rounds of work went into this on the theory that
//!   14 ms was the GPU-to-CPU transfer. It is 0.77 ms.
//!
//! The real path to hardware decode on Windows is D3D11VA directly —
//! `ID3D11VideoDevice::CreateVideoDecoder` plus `DecoderBeginFrame` /
//! `SubmitDecoderBuffers` / `DecoderEndFrame`, fed `DXVA_PicParams_H264` built
//! from a parsed SPS, PPS and slice header. That is what ffmpeg's `d3d11va`,
//! Moonlight and Sunshine all do, and it is why they get sub-millisecond
//! decodes where this gets eleven. It is a large piece of work and it is not
//! this file.
//!
//! ## Encoders are async, decoders usually are not
//!
//! Hardware *encoder* MFTs are almost always asynchronous. Hardware *decoder*
//! MFTs commonly are not, and the two need different drive loops. This supports
//! both: the async event pump when the transform declares itself async, and a
//! plain feed-then-drain otherwise.
//!
//! ## Stream changes are routine here
//!
//! Unlike the encoder, a decoder does not know the picture size until it has
//! parsed a sequence header. `MF_E_TRANSFORM_STREAM_CHANGE` at the start of a
//! stream is therefore normal rather than a fault, and must be answered by
//! re-accepting the output type — ignoring it leaves the transform refusing to
//! produce anything.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use tracing::*;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;

use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use super::codec_h264::DecodedPicture;
use super::codec_mf::{ensure_mf_started, friendly_name, nv12_len, ROWS_PER_WORKER};

/// H.264 decoding on the GPU.
pub struct MediaFoundationDecoder {
    transform: IMFTransform,
    /// The activation object that produced `transform`.
    ///
    /// Held only so it can be shut down on the way out: releasing the transform
    /// alone leaves the decode session allocated, because the activate caches
    /// the object it created. See `codec_mf::ActivatedTransform`.
    activate: IMFActivate,
    events: Option<IMFMediaEventGenerator>,
    /// The D3D11 device the transform decodes on, and the manager that hands it
    /// over. Both are held only to keep them alive — dropping either pulls the
    /// GPU out from under a transform that is still using it.
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    _manager: IMFDXGIDeviceManager,
    /// CPU-readable NV12 the decoded surface is copied into.
    ///
    /// Built lazily, because the picture size is not known until the decoder has
    /// parsed a sequence header. See [`MediaFoundationDecoder::read_surface`]
    /// for why the copy exists at all.
    staging: Option<(ID3D11Texture2D, u16, u16)>,
    width: u16,
    height: u16,
    pending_input_requests: u32,
    /// Reusable RGBA target, so a 1600p picture is not reallocated each frame.
    rgba: Vec<u8>,
    /// Reusable tightly-packed NV12, for the same reason. A DXVA surface has a
    /// row pitch that is almost never the picture width, so the planes are
    /// unpadded into here before conversion.
    packed: Vec<u8>,
    /// Whether `staging` holds a copy that has been issued but not yet read.
    ///
    /// # The pipelining is on the copy, not the picture
    ///
    /// Mapping a staging texture blocks until the GPU has finished writing it.
    /// Issuing a copy and mapping it in the same breath therefore stalls on
    /// your own copy and measures exactly as slow as reading the surface
    /// directly — 14 to 20 ms, which is what the first attempt at this did.
    ///
    /// So the copy for frame N is issued at the end of the call that returns
    /// frame N-1's picture, giving the GPU a whole frame period to complete it
    /// before anything maps it. One frame of latency buys the difference
    /// between a wait and a memcpy.
    copy_in_flight: bool,
    /// Set when the last sample was system memory, so there is nothing staged.
    direct: bool,
    /// GPU colour conversion, cached against the size it was built for.
    ///
    /// Built lazily for the same reason as `staging`: the picture size is not
    /// known until the decoder has parsed a sequence header. `None` means either
    /// "not built yet" or "this GPU would not do it", and both are handled by
    /// falling back to [`nv12_to_rgba`].
    converter: Option<(super::decode_rgba::RgbaConverter, u16, u16)>,
    /// Whether the frame in flight was handed to the converter or to `staging`.
    gpu_convert: bool,
    /// Set once this GPU has declined the conversion, so it is asked only once.
    converter_refused: bool,
    stage_wrap: std::time::Duration,
    stage_input: std::time::Duration,
    stage_output: std::time::Duration,
    /// `ProcessOutput` calls that produced nothing — the tail of the drain loop.
    stage_drain: std::time::Duration,
    stage_copy: std::time::Duration,
    stage_map: std::time::Duration,
    stage_rgba: std::time::Duration,
    staged_frames: u32,
    staged_at: std::time::Instant,
    /// Unused now that the copy is what gets pipelined; see `copy_in_flight`.
    ///
    /// # Why the newest picture is not the one returned
    ///
    /// Mapping a DXVA surface makes the CPU wait for the GPU to finish decoding
    /// it, then stages it into system memory. Measured at **18 ms per frame** on
    /// this machine for a decode that itself costs two or three — the wait, not
    /// the work. It capped the guest at 55 fps and was the "Slow decoding" the
    /// host kept adapting to.
    ///
    /// Holding one frame back gives the GPU a whole frame period to finish
    /// before anything maps it, which turns the stall into a plain copy. The
    /// cost is one frame of latency, and it very nearly pays for itself: the
    /// stall was already latency, it just showed up as the receive task
    /// blocking instead of as a queued picture.
    pub name: String,
}

// SAFETY: as for the encoder — driven from exactly one thread (the guest's
// receive task) and never shared.
unsafe impl Send for MediaFoundationDecoder {}

/// Name of this machine's hardware H.264 decoder, if it has one.
/// The answer to "does this machine have a GPU decoder", worked out once.
static HARDWARE_DECODER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Look for a hardware H.264 decoder without committing to using it.
///
/// Cached for the same reason as [`super::codec_mf::probe`], and pre-emptively:
/// answering means activating a transform, which loads the vendor driver. That
/// cost is invisible at the call site and reads like a getter — which is
/// exactly how the encoder's version ended up being called twice a frame from
/// the settings panel, dropping the whole window to under 3 fps. There is no
/// UI calling this one yet. There will be.
pub fn probe_decoder() -> Option<String> {
    HARDWARE_DECODER
        .get_or_init(|| {
            ensure_mf_started();
            enumerate_decoder().ok().flatten().map(|(_, _, name)| name)
        })
        .clone()
}

/// Enumerate H.264 decoder transforms, returning the first that activates.
///
/// # This finds nothing, and that is a bug in what it asks for
///
/// `MFT_ENUM_FLAG_HARDWARE` selects transforms a vendor registered as hardware.
/// **No such transform exists for H.264 decode on Windows** — not on the AMD
/// machine this was written on, and not on an RTX 3050 Ti. The only registered
/// decoder is the Microsoft H264 Video Decoder MFT, as a *sync* transform.
///
/// It was assumed that transform *was* the GPU path — that it decodes through
/// DXVA once sent `MFT_MESSAGE_SET_D3D_MANAGER`. **It does not.** It accepts the
/// manager and decodes on the CPU anyway; see the module header for the
/// measurements. So SYNCMFT is still the flag that matters, because it is the
/// only one that returns anything, but what it returns is a software decoder.
///
/// So every guest falls back to OpenH264 on the CPU, at whatever resolution the
/// host is sending. Run `what_h264_decoders_this_machine_registers` to see the
/// enumeration for yourself.
///
/// Fixing this means using the Microsoft transform and giving it a device
/// manager, the mirror of what [`super::capture_nv12`] does on the capture side.
fn enumerate_decoder() -> Result<Option<(IMFTransform, IMFActivate, String)>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: MFTEnumEx writes an array of IMFActivate we take ownership of.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            // **SYNCMFT is the one that matters.** See the note above: the
            // hardware and async classes are empty for H.264 on Windows, and
            // asking only for those is what made every guest fall back to the
            // CPU. They stay in the mask because a machine that does register a
            // vendor transform should get it, and SORTANDFILTER puts hardware
            // first when both are present.
            MFT_ENUM_FLAG_HARDWARE
                | MFT_ENUM_FLAG_ASYNCMFT
                | MFT_ENUM_FLAG_SYNCMFT
                | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx failed for decoders")?;
    }

    if activates.is_null() || count == 0 {
        return Ok(None);
    }

    // SAFETY: MFTEnumEx guarantees `count` initialised entries, and taking
    // ownership is what releases them — see `codec_mf::owned_activates` for the
    // encode-session leak that borrowing them instead used to cause.
    let items = unsafe { super::codec_mf::owned_activates(activates, count) };
    let mut found = None;
    for activate in items.iter() {
        let name = friendly_name(activate);
        // SAFETY: activating a transform we just enumerated.
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                // The activate is kept alongside the transform so shutting it
                // down is possible later; releasing the transform alone does
                // not give a decode session back.
                found = Some((transform, activate.clone(), name));
                break;
            }
            Err(e) => debug!("Could not activate decoder {name}: {e}"),
        }
    }

    Ok(found)
}

/// Copy a mapped NV12 staging texture into `out`, tightly packed.
///
/// The row pitch is almost never the picture width, and the chroma plane starts
/// at `RowPitch * height` — reading the mapping as one block shears the luma and
/// misplaces the chroma, which looks like a picture torn into diagonal stripes
/// rather than like an error.
fn unpack_nv12(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u16,
    height: u16,
    out: &mut [u8],
) -> Result<()> {
    let (w, h) = (width as usize, height as usize);
    let pitch = mapped.RowPitch as usize;
    anyhow::ensure!(pitch >= w, "NV12 pitch {pitch} is narrower than {w} pixels");
    anyhow::ensure!(!mapped.pData.is_null(), "NV12 mapping has no data");
    anyhow::ensure!(out.len() >= nv12_len(width, height), "NV12 target is too small");

    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let base = mapped.pData as *const u8;
    for row in 0..h {
        // SAFETY: row < h, inside a mapping of at least `pitch * h` bytes.
        let src = unsafe { base.add(row * pitch) };
        // SAFETY: exactly `w` bytes into a slice of that length.
        unsafe { std::ptr::copy_nonoverlapping(src, y_plane[row * w..].as_mut_ptr(), w) };
    }
    for row in 0..h.div_ceil(2) {
        // SAFETY: the UV plane follows the luma plane at `pitch * h`.
        let src = unsafe { base.add(pitch * h + row * pitch) };
        // SAFETY: as above.
        unsafe { std::ptr::copy_nonoverlapping(src, uv_plane[row * w..].as_mut_ptr(), w) };
    }
    Ok(())
}

/// Copy one NV12 buffer into `out`, tightly packed.
///
/// Two shapes to handle, and which one arrives depends on whether the transform
/// is decoding on the GPU. A DXVA surface is a **2D buffer**: its rows are
/// padded to a hardware-friendly pitch, and reading it as if it were contiguous
/// shears the luma and puts the chroma plane in the wrong place — a picture in
/// diagonal stripes, not an error. A CPU decode hands back a flat buffer where
/// the pitch happens to equal the width.
///
/// `IMF2DBuffer` is asked for first because it is the one that reports the
/// pitch; the flat path is the fallback for transforms that do not implement it.
///
/// # Safety
/// Caller must be inside an `unsafe` block; this only wraps COM calls on a
/// buffer the transform produced.
unsafe fn read_nv12(
    buffer: &IMFMediaBuffer,
    width: u16,
    height: u16,
    out: &mut [u8],
) -> Result<()> {
    let (w, h) = (width as usize, height as usize);
    anyhow::ensure!(out.len() >= nv12_len(width, height), "NV12 target is too small");

    if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
        let mut scanline = std::ptr::null_mut();
        let mut pitch = 0i32;
        // SAFETY: both out-params are valid; unlocked on every path below.
        unsafe { two_d.Lock2D(&mut scanline, &mut pitch) }.context("Lock2D failed")?;
        let result = (|| {
            let pitch = pitch as usize;
            anyhow::ensure!(pitch >= w, "NV12 pitch {pitch} is narrower than {w} pixels");
            anyhow::ensure!(!scanline.is_null(), "Lock2D returned no data");
            let (y_plane, uv_plane) = out.split_at_mut(w * h);
            for row in 0..h {
                // SAFETY: row < h, inside the locked surface.
                let src = unsafe { scanline.add(row * pitch) };
                // SAFETY: exactly `w` bytes into a slice of that length.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, y_plane[row * w..].as_mut_ptr(), w)
                };
            }
            for row in 0..h.div_ceil(2) {
                // SAFETY: the UV plane follows the luma plane at `pitch * h`.
                let src = unsafe { scanline.add(pitch * h + row * pitch) };
                // SAFETY: as above.
                unsafe {
                    std::ptr::copy_nonoverlapping(src, uv_plane[row * w..].as_mut_ptr(), w)
                };
            }
            Ok(())
        })();
        // SAFETY: paired with the successful Lock2D above.
        unsafe { two_d.Unlock2D() }.ok();
        return result;
    }

    let mut ptr = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: out-params are valid; unlocked below.
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) }.context("Lock failed")?;
    // SAFETY: the transform reports `len` readable bytes at `ptr`.
    let nv12 = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let complete = nv12.len() >= nv12_len(width, height);
    if complete {
        out[..nv12_len(width, height)].copy_from_slice(&nv12[..nv12_len(width, height)]);
    }
    // SAFETY: paired with the successful Lock above.
    unsafe { buffer.Unlock() }.ok();
    anyhow::ensure!(complete, "decoder returned a short NV12 buffer");
    Ok(())
}

/// A D3D11 device wrapped in the manager Media Foundation wants.
///
/// Video support is the point of the device; multithread protection is
/// non-negotiable because the transform touches it from its own worker threads
/// while the guest's receive task drives `ProcessInput`, and a driver told it
/// has exclusive access will corrupt frames or drop the device instead of
/// reporting anything.
fn d3d_manager() -> Result<(ID3D11Device, ID3D11DeviceContext, IMFDXGIDeviceManager)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: out-param is valid; failure leaves it None.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("no D3D11 device with video support for decoding")?;
    let device = device.context("D3D11CreateDevice returned nothing")?;
    let context = context.context("D3D11 device came back without a context")?;

    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: the device is live; this only sets a flag on it.
        unsafe { let _ = mt.SetMultithreadProtected(true); };
    }

    let mut token = 0u32;
    let mut manager = None;
    // SAFETY: both out-params are valid for the call.
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .context("MFCreateDXGIDeviceManager failed")?;
    let manager = manager.context("MFCreateDXGIDeviceManager returned nothing")?;
    // SAFETY: handing the manager a device it will hold a reference to.
    unsafe { manager.ResetDevice(&device, token) }
        .context("could not attach the D3D11 device to the manager")?;

    Ok((device, context, manager))
}

impl MediaFoundationDecoder {
    /// Build a decoder, or fail if this machine has no hardware one.
    pub fn new() -> Result<Self> {
        ensure_mf_started();
        let (transform, activate, name) =
            enumerate_decoder()?.context("no H.264 decoder transform on this machine")?;
        let (device, context, manager) = d3d_manager()?;

        // **This is what makes it a GPU decode.** The Microsoft transform runs
        // on the CPU until it is handed a device manager, at which point it
        // decodes through DXVA on whatever GPU that device belongs to. Sent
        // before any type is negotiated, because it changes which output types
        // the transform will offer.
        //
        // SAFETY: the manager outlives the transform; the pointer is passed by
        // value as the message parameter, as the API requires.
        unsafe {
            let param = manager.as_raw() as usize;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, param)
                .context("the decoder refused a D3D11 device manager")?;
        }

        let mut decoder = Self {
            transform,
            activate,
            events: None,
            device,
            context,
            _manager: manager,
            staging: None,
            width: 0,
            height: 0,
            pending_input_requests: 0,
            rgba: Vec::new(),
            packed: Vec::new(),
            copy_in_flight: false,
            direct: false,
            converter: None,
            gpu_convert: false,
            converter_refused: false,
            stage_wrap: std::time::Duration::ZERO,
            stage_input: std::time::Duration::ZERO,
            stage_output: std::time::Duration::ZERO,
            stage_drain: std::time::Duration::ZERO,
            stage_copy: std::time::Duration::ZERO,
            stage_map: std::time::Duration::ZERO,
            stage_rgba: std::time::Duration::ZERO,
            staged_frames: 0,
            staged_at: std::time::Instant::now(),
            name,
        };
        decoder.configure()?;
        info!("H.264 decoding via {} — on the CPU; see enumerate_decoder", decoder.name);
        Ok(decoder)
    }

    fn configure(&mut self) -> Result<()> {
        // SAFETY: documented IMFTransform setup on an object we own.
        unsafe {
            let attrs = self.transform.GetAttributes().ok();
            let is_async = attrs
                .as_ref()
                .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok())
                .unwrap_or(0)
                == 1;
            if is_async {
                if let Some(ref attrs) = attrs {
                    attrs
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .context("could not unlock the async decoder")?;
                }
                self.events = self.transform.cast::<IMFMediaEventGenerator>().ok();
            }
            if let Some(ref attrs) = attrs {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

                // # What does *not* help, so nobody tries it twice
                //
                // `MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT` was set here on the theory
                // that the decoder was waiting for this code to release its
                // output surface. Swept 1, 4, 6 and 16 against the benchmark in
                // this file: 11.3, 11.5, 11.9, 11.5 ms. It is not a pool depth
                // problem. See `configure` below for what it actually is.
            }

            // Input type first — the reverse of the encoder, because a decoder's
            // output cannot be described until the stream has been.
            let input = MFCreateMediaType().context("MFCreateMediaType")?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

            self.transform
                .SetInputType(0, &input, 0)
                .context("the decoder rejected the H.264 input type")?;

            self.accept_nv12_output()?;
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    /// Take the first NV12 output type the transform offers, and read its size.
    ///
    /// # Safety
    /// Caller must be inside an `unsafe` block; this only wraps COM calls on a
    /// transform we own.
    unsafe fn accept_nv12_output(&mut self) -> Result<()> {
        for index in 0.. {
            // SAFETY: enumerating output types is a read-only query.
            let Ok(candidate) = (unsafe { self.transform.GetOutputAvailableType(0, index) }) else {
                break;
            };
            // SAFETY: reading attributes of a type the transform just gave us.
            let is_nv12 =
                unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12);
            if !is_nv12 {
                continue;
            }
            // SAFETY: setting a type the transform itself offered.
            unsafe {
                self.transform
                    .SetOutputType(0, &candidate, 0)
                    .context("the decoder rejected its own NV12 output type")?;
                let size = candidate.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                if size != 0 {
                    self.width = (size >> 32) as u16;
                    self.height = size as u16;
                }
            }
            return Ok(());
        }
        anyhow::bail!("decoder offers no NV12 output type")
    }

    /// Decode one Annex-B packet into RGBA, or `None` if no picture came out.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        if packet.is_empty() {
            return Ok(None);
        }
        let started = std::time::Instant::now();
        let sample = self.wrap(packet)?;
        self.stage_wrap += started.elapsed();
        match self.events.clone() {
            Some(events) => self.feed_async(&events, sample),
            None => self.feed_sync(sample),
        }
    }

    /// Copy a packet into a media buffer the transform can read.
    fn wrap(&self, packet: &[u8]) -> Result<IMFSample> {
        // SAFETY: allocating a buffer and filling the region it hands back.
        unsafe {
            let buffer = MFCreateMemoryBuffer(packet.len() as u32)?;
            let mut dst = std::ptr::null_mut();
            let mut max = 0u32;
            buffer.Lock(&mut dst, Some(&mut max), None)?;
            if (max as usize) < packet.len() {
                let _ = buffer.Unlock();
                anyhow::bail!("media buffer is {max} bytes, need {}", packet.len());
            }
            std::ptr::copy_nonoverlapping(packet.as_ptr(), dst, packet.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(packet.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        }
    }

    /// Report where the last second of decode time actually went.
    ///
    /// Written because three rounds of reasoning about which stage was slow
    /// produced three wrong answers. The stages are cheap to time and the
    /// argument is not.
    fn report_stages(&mut self) {
        if self.staged_at.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        let n = self.staged_frames.max(1) as f64;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n;
        info!(
            "decode stages over {} frames ({}): wrap {:.2}, input {:.2}, output {:.2}, \
             drain {:.2}, copy {:.2}, map {:.2}, rgba {:.2} ms",
            self.staged_frames,
            if self.gpu_convert { "GPU colour" } else { "CPU colour" },
            ms(self.stage_wrap),
            ms(self.stage_input),
            ms(self.stage_output),
            ms(self.stage_drain),
            ms(self.stage_copy),
            ms(self.stage_map),
            ms(self.stage_rgba),
        );
        self.stage_wrap = std::time::Duration::ZERO;
        self.stage_input = std::time::Duration::ZERO;
        self.stage_output = std::time::Duration::ZERO;
        self.stage_drain = std::time::Duration::ZERO;
        self.stage_copy = std::time::Duration::ZERO;
        self.stage_map = std::time::Duration::ZERO;
        self.stage_rgba = std::time::Duration::ZERO;
        self.staged_frames = 0;
        self.staged_at = std::time::Instant::now();
    }

    fn feed_sync(&mut self, sample: IMFSample) -> Result<Option<DecodedPicture>> {
        // SAFETY: documented synchronous MFT flow. A decoder refusing input
        // because it has output waiting is normal, not a failure.
        let t0 = std::time::Instant::now();
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {}
            Err(e) => return Err(anyhow::anyhow!("decoder rejected a packet: {e}")),
        }
        self.stage_input += t0.elapsed();

        // Drain fully: the newest picture wins, because an older one is already
        // stale by the time a later one exists and drawing it only adds latency.
        let mut picture = None;
        while let Some(decoded) = self.take_picture()? {
            picture = Some(decoded);
        }
        self.staged_frames += 1;
        self.report_stages();
        Ok(picture)
    }

    fn feed_async(
        &mut self,
        events: &IMFMediaEventGenerator,
        sample: IMFSample,
    ) -> Result<Option<DecodedPicture>> {
        let mut picture = None;

        // Same shape as the encoder's pump, for the same reasons: block only for
        // permission to feed, then drain without waiting on output that may
        // require the next packet to exist.
        while self.pending_input_requests == 0 {
            // SAFETY: blocking GetEvent on a generator we own.
            let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NONE) }
                .context("waiting for the decoder to request input")?;
            self.handle_event(&event, &mut picture)?;
        }
        self.pending_input_requests -= 1;
        // SAFETY: feeding against a request the transform issued.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .context("the decoder rejected a packet")?;

        loop {
            // SAFETY: non-blocking poll of the same generator.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event, &mut picture)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(anyhow::anyhow!("decoder event queue failed: {e}")),
            }
        }
        Ok(picture)
    }

    fn handle_event(
        &mut self,
        event: &IMFMediaEvent,
        picture: &mut Option<DecodedPicture>,
    ) -> Result<()> {
        // SAFETY: reading the type of an event we hold.
        let kind = unsafe { event.GetType() }?;
        if kind == METransformNeedInput.0 as u32 {
            self.pending_input_requests += 1;
        } else if kind == METransformHaveOutput.0 as u32
            && let Some(decoded) = self.take_picture()?
        {
            // Newest wins: an older picture is already stale by the time a later
            // one exists, and drawing it would only add latency.
            *picture = Some(decoded);
        }
        Ok(())
    }

    /// Pull one decoded picture, converting NV12 to RGBA.
    fn take_picture(&mut self) -> Result<Option<DecodedPicture>> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        // SAFETY: `buffers` is initialised above; both ManuallyDrop fields are
        // taken exactly once afterwards, which releases whatever the transform
        // put there.
        let t_out = std::time::Instant::now();
        let (result, sample) = unsafe {
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
            (result, sample)
        };

        // Split, because the drain loop calls this twice per frame and the two
        // calls answer different questions: one decodes, one only establishes
        // that there is nothing left. Charging both to `output` hid which.
        let spent = t_out.elapsed();
        match result {
            Ok(()) => self.stage_output += spent,
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                self.stage_drain += spent;
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.stage_drain += spent;
                // Routine here, unlike on the encoder: the decoder has just
                // worked out the real picture size. Ignoring it leaves the
                // transform refusing to produce anything at all.
                // SAFETY: the documented response to a stream change.
                unsafe { self.accept_nv12_output() }?;
                // The held surface is the old size, and converting it against
                // the new one would read past the end of a plane.
                self.copy_in_flight = false;
                debug!("Decoder negotiated {}x{}", self.width, self.height);
                return Ok(None);
            }
            Err(e) => return Err(anyhow::anyhow!("decoder ProcessOutput failed: {e}")),
        }

        let Some(sample) = sample else { return Ok(None) };

        // Read the copy issued last time round — the GPU has had a full frame
        // period to finish it, so this is a plain read rather than a wait.
        let picture = if self.copy_in_flight {
            Some(self.convert()?)
        } else {
            // First frame of a stream produces no picture, which every caller
            // already handles: `Ok(None)` is the routine "still buffering"
            // answer.
            None
        };

        // Then start the next one, and let it run while the caller does
        // something else with the picture above.
        let t_copy = std::time::Instant::now();
        self.copy_in_flight = self.issue_copy(&sample).is_ok();
        self.stage_copy += t_copy.elapsed();
        Ok(picture)
    }

    /// Copy one decoded surface into `self.packed`, tightly packed.
    ///
    /// # Why this does not simply lock the buffer
    ///
    /// A DXVA output surface lives in memory the GPU writes and the CPU is
    /// terrible at reading — uncached, write-combined, and read a byte at a
    /// time by any memcpy that touches it. `ConvertToContiguousBuffer` then
    /// copies it *again* on the way out. Together those measured **14 ms per
    /// frame** for a decode costing two or three, and capped the guest at
    /// 55 fps.
    ///
    /// Copying into a `D3D11_USAGE_STAGING` texture first is the documented
    /// fast path: the driver does a GPU-side blit into cacheable system memory,
    /// and the map that follows is an ordinary read. It is the same move
    /// [`super::capture_nv12`] makes on the capture side, in reverse.
    ///
    /// Falls back to locking the buffer directly if the sample is not
    /// D3D-backed, which is what a software decode hands back.
    fn issue_copy(&mut self, sample: &IMFSample) -> Result<()> {
        // SAFETY: reading a sample the transform produced. Every COM call is
        // checked and every lock is released on the path that took it.
        let dxgi = unsafe { sample.GetBufferByIndex(0) }
            .ok()
            .and_then(|buffer| buffer.cast::<IMFDXGIBuffer>().ok());

        let Some(dxgi) = dxgi else {
            // A software decode hands back system memory, which has nothing to
            // wait for — read it straight into place and report no copy in
            // flight, so the next call converts from `packed` as it stands.
            // SAFETY: as above; this path locks the buffer directly.
            unsafe {
                let buffer = sample.ConvertToContiguousBuffer()?;
                read_nv12(&buffer, self.width, self.height, &mut self.packed)?;
            }
            self.direct = true;
            return Ok(());
        };
        self.direct = false;

        // SAFETY: GetResource writes an interface pointer we take ownership of,
        // for the IID we ask for.
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
            .context("decoded surface is not a D3D11 texture")?;
        anyhow::ensure!(!raw.is_null(), "GetResource returned nothing");
        // SAFETY: `raw` is a live ID3D11Texture2D with a reference we now own.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        // SAFETY: a DXGI buffer always knows its own subresource.
        let slice = unsafe { dxgi.GetSubresourceIndex() }.unwrap_or(0);

        // Convert on the GPU if this driver will, which turns the readback into
        // exactly the bytes the window uploads and deletes the 4.9 ms the CPU
        // conversion cost. Both paths queue their work rather than waiting.
        self.gpu_convert = false;
        if self.ensure_converter()
            && let Some((converter, _, _)) = self.converter.as_mut()
        {
            match converter.submit(&texture, slice) {
                Ok(()) => {
                    self.gpu_convert = true;
                    return Ok(());
                }
                // Retrying a blit this driver has already refused would cost
                // more per frame than the CPU conversion it is meant to save.
                Err(e) => {
                    warn!("GPU colour conversion failed ({e:#}) — using the CPU from here");
                    self.converter = None;
                    self.converter_refused = true;
                }
            }
        }

        let staging = self.staging_texture()?;
        // SAFETY: staging matches the surface in format and size. The copy is
        // queued, not waited on — that is the entire point.
        unsafe {
            self.context
                .CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, slice, None);
            // Nudges the driver to start work now rather than at the next
            // natural flush, so the frame period is spent copying.
            self.context.Flush();
        }
        Ok(())
    }

    /// Build the NV12-to-RGBA converter for the current size, if it is missing.
    ///
    /// Returns whether one is available. A GPU that refuses the conversion is
    /// logged once and then left alone: retrying per frame would spend more on
    /// failing than the CPU path costs.
    fn ensure_converter(&mut self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        match &self.converter {
            Some((_, w, h)) if (*w, *h) == (self.width, self.height) => return true,
            // A size change means a new stream geometry; the old processor is
            // built for the old size and would blit the wrong rectangle.
            Some(_) => self.converter = None,
            None if self.converter_refused => return false,
            None => {}
        }
        match super::decode_rgba::RgbaConverter::new(
            &self.device,
            &self.context,
            self.width,
            self.height,
        ) {
            Ok(converter) => {
                self.converter = Some((converter, self.width, self.height));
                true
            }
            Err(e) => {
                warn!("No GPU colour conversion for decoded frames ({e:#}) — using the CPU");
                self.converter_refused = true;
                false
            }
        }
    }

    /// Read whatever copy is sitting in the staging texture.
    fn drain_staging(&mut self) -> Result<()> {
        if self.direct {
            // Already in `packed`, straight from system memory.
            return Ok(());
        }
        let staging = self.staging_texture()?;
        // SAFETY: mapping a staging texture we own; unmapped on both paths.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not map the decoded NV12 staging texture")?;
            let result = unpack_nv12(&mapped, self.width, self.height, &mut self.packed);
            self.context.Unmap(&staging, 0);
            result
        }
    }

    /// The staging texture for the current picture size, built on demand.
    fn staging_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some((texture, w, h)) = &self.staging
            && (*w, *h) == (self.width, self.height)
        {
            return Ok(texture.clone());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width as u32,
            Height: self.height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: desc is fully initialised; the out-param is checked below.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .context("could not create the decoder's staging texture")?;
        let texture = texture.context("CreateTexture2D returned nothing")?;
        self.staging = Some((texture.clone(), self.width, self.height));
        Ok(texture)
    }

    /// Turn one NV12 sample into the RGBA the renderer uploads.
    fn convert(&mut self) -> Result<DecodedPicture> {
        if self.width == 0 || self.height == 0 {
            // SAFETY: querying the transform's current output type.
            unsafe { self.accept_nv12_output() }?;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        anyhow::ensure!(w > 0 && h > 0, "decoder produced a zero-sized picture");

        self.rgba.resize(w * h * 4, 0);

        if self.gpu_convert {
            // The video processor already produced RGBA in exactly the byte
            // order the window wants, so the readback *is* the conversion.
            let t_map = std::time::Instant::now();
            // Two disjoint fields, which is why this is not a method call.
            let read = match self.converter.as_mut() {
                Some((converter, _, _)) => converter.read(&mut self.rgba),
                None => Err(anyhow::anyhow!("converter vanished between submit and read")),
            };
            self.stage_map += t_map.elapsed();
            read?;
        } else {
            self.packed.resize(nv12_len(self.width, self.height), 0);
            let t_map = std::time::Instant::now();
            self.drain_staging()?;
            self.stage_map += t_map.elapsed();
            let t_rgba = std::time::Instant::now();
            nv12_to_rgba(&self.packed, self.width, self.height, &mut self.rgba);
            self.stage_rgba += t_rgba.elapsed();
        }

        Ok(DecodedPicture { width: w, height: h, rgba: std::mem::take(&mut self.rgba) })
    }
}

impl Drop for MediaFoundationDecoder {
    fn drop(&mut self) {
        // SAFETY: telling the transform to stop, then shutting down the object
        // the activate created. The shutdown is the part that gives the decode
        // session back — dropping the transform reference does not.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// NV12 to RGBA, split across threads.
///
/// The inverse of the encoder's converter and threaded for the same reason: at
/// 1600p it is four million pixels of scalar arithmetic sitting on the guest's
/// critical path, and leaving it on one core is how a fast machine still reports
/// as "slow decoding".
pub fn nv12_to_rgba(nv12: &[u8], width: u16, height: u16, out: &mut [u8]) {
    let (w, h) = (width as usize, height as usize);
    debug_assert!(nv12.len() >= nv12_len(width, height));
    debug_assert!(out.len() >= w * h * 4);

    let (y_plane, uv_plane) = nv12.split_at(w * h);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(h.div_ceil(ROWS_PER_WORKER))
        .max(1);
    let band = h.div_ceil(workers);

    let bands = out[..w * h * 4].chunks_mut(band * w * 4).enumerate();
    if workers == 1 {
        for (index, dst) in bands {
            rgba_band(y_plane, uv_plane, w, index * band, dst);
        }
        return;
    }
    std::thread::scope(|scope| {
        for (index, dst) in bands {
            scope.spawn(move || rgba_band(y_plane, uv_plane, w, index * band, dst));
        }
    });
}

/// Convert one horizontal band of NV12 into RGBA.
fn rgba_band(y_plane: &[u8], uv_plane: &[u8], w: usize, first_row: usize, out: &mut [u8]) {
    for local in 0..out.len() / (w * 4) {
        let y_row = first_row + local;
        let uv_row = (y_row / 2) * w;
        for x in 0..w {
            // BT.601 limited range — the inverse of the coefficients the encoder
            // side uses. The two must agree or the picture shifts colour
            // depending on which decoder happened to run.
            let y = (y_plane[y_row * w + x] as i32 - 16) * 298;
            let chroma = uv_row + (x & !1);
            let u = uv_plane[chroma] as i32 - 128;
            let v = uv_plane[chroma + 1] as i32 - 128;

            let i = (local * w + x) * 4;
            out[i] = ((y + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            out[i + 1] = ((y - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            out[i + 2] = ((y + 516 * u + 128) >> 8).clamp(0, 255) as u8;
            out[i + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an NV12 frame of a known flat colour.
    fn flat_nv12(width: u16, height: u16, y: u8, u: u8, v: u8) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut data = vec![y; w * h];
        for _ in 0..(w * h / 2) / 2 {
            data.push(u);
            data.push(v);
        }
        data.resize(nv12_len(width, height), 128);
        data
    }

    #[test]
    fn a_grey_frame_decodes_to_grey() {
        // Y=128 with neutral chroma is mid grey. Getting this wrong by swapping
        // U and V, or by using full-range coefficients, tints every session.
        let (w, h) = (64u16, 32u16);
        let nv12 = flat_nv12(w, h, 128, 128, 128);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        nv12_to_rgba(&nv12, w, h, &mut rgba);

        for px in rgba.chunks_exact(4) {
            assert!(
                px[0].abs_diff(px[1]) <= 2 && px[1].abs_diff(px[2]) <= 2,
                "neutral chroma must stay grey, got {px:?}"
            );
            assert!((125..=135).contains(&px[0]), "expected mid grey, got {}", px[0]);
            assert_eq!(px[3], 255, "alpha must be opaque");
        }
    }

    #[test]
    fn the_conversion_round_trips_against_the_encoder_side() {
        // The two converters must agree, or a picture changes colour depending
        // on which encoder and decoder happened to be chosen at each end.
        let (w, h) = (64u16, 32u16);
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            px[0] = 40; // B
            px[1] = 160; // G
            px[2] = if i % 2 == 0 { 200 } else { 90 }; // R
            px[3] = 255;
        }

        let mut nv12 = Vec::new();
        super::super::codec_mf::bgra_to_nv12(&bgra, w, h, &mut nv12);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        nv12_to_rgba(&nv12, w, h, &mut rgba);

        // 4:2:0 averages chroma over 2x2 blocks, so per-pixel colour cannot come
        // back exactly. The frame average is what must survive.
        let mean = |data: &[u8], offset: usize, stride: usize| {
            data.chunks_exact(stride).map(|px| px[offset] as u32).sum::<u32>()
                / (data.len() / stride) as u32
        };
        assert!(mean(&rgba, 0, 4).abs_diff(mean(&bgra, 2, 4)) < 24, "red drifted");
        assert!(mean(&rgba, 1, 4).abs_diff(mean(&bgra, 1, 4)) < 24, "green drifted");
        assert!(mean(&rgba, 2, 4).abs_diff(mean(&bgra, 0, 4)) < 24, "blue drifted");
    }

    /// Every H.264 decoder this machine registers, and how each is flagged.
    ///
    /// Run this before believing anything about hardware decoding. On the
    /// machine it was written on — and, from the logs, on an RTX 3050 Ti too —
    /// the answer is:
    ///
    /// ```text
    /// HARDWARE             (none)
    /// HARDWARE|ASYNCMFT    (none)
    /// ASYNCMFT             (none)
    /// SYNCMFT              Microsoft H264 Video Decoder MFT
    /// ```
    ///
    /// Which is why [`probe_decoder`] finds nothing: **Windows does not ship a
    /// vendor hardware MFT for H.264 decode.** GPU decoding goes through the
    /// Microsoft transform above, which uses DXVA once it is handed a D3D11
    /// device manager — and this module never hands it one.
    /// First decoder transform matching this type and flag set, by name.
    fn enumerate_named(want: &MFT_REGISTER_TYPE_INFO, flag: MFT_ENUM_FLAG) -> Option<String> {
        let mut activates = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: read-only enumeration; the array is freed below.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flag | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(want),
                None,
                &mut activates,
                &mut count,
            )
            .ok()?;
        }
        if count == 0 || activates.is_null() {
            return None;
        }
        // SAFETY: MFTEnumEx wrote `count` interface pointers we now own.
        let list = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let name = list.first().and_then(|a| a.as_ref()).map(friendly_name);
        for entry in list {
            // SAFETY: dropping each reference exactly once.
            unsafe { drop(std::ptr::read(entry)) };
        }
        // SAFETY: freeing the array MFTEnumEx allocated with CoTaskMemAlloc.
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
        name
    }

    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn what_h264_decoders_this_machine_registers() {
        ensure_mf_started();

        // HEVC and AV1 as well as H.264: if a vendor registers a *hardware*
        // transform for one of those, switching codec is far cheaper than
        // writing a D3D11VA decoder by hand.
        for (label, subtype) in [
            ("H264", MFVideoFormat_H264),
            ("HEVC", MFVideoFormat_HEVC),
            ("AV1", MFVideoFormat_AV1),
        ] {
            let want = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: subtype,
            };
            for (flag_name, flag) in [
                ("HARDWARE", MFT_ENUM_FLAG_HARDWARE),
                ("HARDWARE|ASYNC", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT),
                ("ALL", MFT_ENUM_FLAG_ALL),
            ] {
                let found = enumerate_named(&want, flag);
                println!("  {label:5} {flag_name:16} {}", found.unwrap_or("(none)".into()));
            }
        }

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };

        let cases: [(&str, MFT_ENUM_FLAG); 5] = [
            ("HARDWARE", MFT_ENUM_FLAG_HARDWARE),
            ("HARDWARE|ASYNCMFT", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT),
            ("ASYNCMFT", MFT_ENUM_FLAG_ASYNCMFT),
            ("SYNCMFT", MFT_ENUM_FLAG_SYNCMFT),
            ("ALL", MFT_ENUM_FLAG_ALL),
        ];

        println!();
        for (label, flags) in cases {
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count: u32 = 0;
            // SAFETY: out-params are valid; the array is freed below.
            let result = unsafe {
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_DECODER,
                    flags | MFT_ENUM_FLAG_SORTANDFILTER,
                    Some(&input),
                    Some(&output),
                    &mut activates,
                    &mut count,
                )
            };
            if result.is_err() || activates.is_null() || count == 0 {
                println!("  {label:<20} (none)");
                continue;
            }
            // SAFETY: MFTEnumEx guarantees `count` initialised entries, and
            // taking ownership releases them.
            let items = unsafe { crate::codec_mf::owned_activates(activates, count) };
            let names: Vec<String> =
                items.iter().map(crate::codec_mf::friendly_name).collect();
            println!("  {label:<20} {}", names.join(", "));
        }
        println!();
    }

    #[test]
    fn report_available_hardware_decoder() {
        match probe_decoder() {
            Some(name) => println!("hardware decoder: {name}"),
            None => println!("no hardware H.264 decoder on this machine"),
        }
    }

    #[test]
    #[ignore = "requires a hardware H.264 decoder"]
    fn a_hardware_encoded_frame_decodes_on_the_gpu() {
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::EncodeConfig;

        let Some(_) = crate::codec_mf::probe() else { return };
        let Ok(mut decoder) = MediaFoundationDecoder::new() else {
            eprintln!("no hardware decoder — skipping");
            return;
        };

        let (w, h) = (640u16, 480u16);
        let config = EncodeConfig { fps: 60, ..EncodeConfig::default() };
        let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("encoder");

        let mut src = vec![0u8; w as usize * h as usize * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px[0] = (i % 200) as u8;
            px[1] = 90;
            px[2] = 210;
            px[3] = 255;
        }

        let mut decoded = 0;
        for _ in 0..30 {
            let (packet, _) = encoder.encode(&src, w, h, crate::PixelFormat::Bgra).expect("encode");
            if packet.is_empty() {
                continue;
            }
            if decoder.decode(&packet).expect("the GPU must decode its own output").is_some() {
                decoded += 1;
            }
        }
        assert!(decoded > 0, "nothing decoded across 30 encoded frames");
    }

    /// What this GPU says it can decode in hardware.
    ///
    /// Run with `cargo test -p tain-core -- --ignored --nocapture decoder_profiles`.
    /// Worth keeping: when a guest reports slow decode, the first question is
    /// whether the hardware was ever able to help, and this answers it without
    /// a debugger.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn decoder_profiles() {
        use windows::Win32::Graphics::Direct3D11::ID3D11VideoDevice;

        let Ok((device, _, _)) = d3d_manager() else {
            eprintln!("no D3D11 device with video support");
            return;
        };
        let Ok(video): Result<ID3D11VideoDevice, _> = device.cast() else {
            eprintln!("device has no ID3D11VideoDevice");
            return;
        };

        // SAFETY: read-only enumeration on a device we own.
        let count = unsafe { video.GetVideoDecoderProfileCount() };
        println!("{count} decoder profiles");
        for i in 0..count {
            // SAFETY: index is below the count just read.
            let Ok(guid) = (unsafe { video.GetVideoDecoderProfile(i) }) else { continue };
            // {1b81be68-a0c7-11d3-b984-00c04f2e73c5}, the standard H.264 VLD
            // profile without film grain. Spelled out because windows-rs does
            // not export the D3D11_DECODER_PROFILE_* constants.
            let h264 = guid
                == windows::core::GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
            // SAFETY: querying support for a profile the device just listed.
            let nv12 = unsafe { video.CheckVideoDecoderFormat(&guid, DXGI_FORMAT_NV12) }
                .map(|b| b.as_bool())
                .unwrap_or(false);
            if h264 {
                println!("  {guid:?}  <-- H264_VLD_NOFGT, NV12 output: {nv12}");
            } else {
                println!("  {guid:?}  NV12 output: {nv12}");
            }
        }
    }

    /// Where decode time goes, at a realistic size, on moving content.
    ///
    /// Run with `cargo test --release -p tain-core -- --ignored --nocapture
    /// decode_stage_cost`. Debug numbers are meaningless here — the colour
    /// conversion fallback alone is an order of magnitude out — and a static
    /// picture is worse than meaningless, because H.264 encodes it to almost
    /// nothing and the decoder never does the work being measured.
    #[test]
    #[ignore = "diagnostic — run explicitly, in release"]
    fn decode_stage_cost() {
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::EncodeConfig;

        let Some(_) = crate::codec_mf::probe() else { return };
        let (w, h) = (1920u16, 1080u16);
        let Ok(mut decoder) = MediaFoundationDecoder::new() else {
            eprintln!("no hardware decoder — skipping");
            return;
        };
        let config = EncodeConfig { fps: 60, ..EncodeConfig::default() };
        let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("encoder");

        let (wu, hu) = (w as usize, h as usize);
        let mut src = vec![0u8; wu * hu * 4];
        let mut decoded = 0u32;

        // Encode everything first, then decode. Interleaving the two puts the
        // encoder's work in the same GPU queue as the decoder's, and what comes
        // out is the sum of both rather than the cost of either.
        // SAFETY: reading an attribute store the transform owns. Printed rather
        // than asserted: it says the transform *can* use D3D11, not that it did.
        let aware = unsafe { decoder.transform.GetAttributes() }
            .ok()
            .and_then(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.ok());
        println!("decoder: {} — MF_SA_D3D11_AWARE = {aware:?}", decoder.name);

        const FRAMES: usize = 120;
        let mut packets = Vec::with_capacity(FRAMES);
        for frame in 0..FRAMES {
            // A moving diagonal gradient: every pixel changes every frame, so
            // the encoder cannot skip and the decoder cannot coast.
            for y in 0..hu {
                for x in (0..wu).step_by(1) {
                    let px = &mut src[(y * wu + x) * 4..][..4];
                    px[0] = (x + y + frame * 7) as u8;
                    px[1] = (y * 2 + frame * 3) as u8;
                    px[2] = (x * 3 + frame * 11) as u8;
                    px[3] = 255;
                }
            }
            let (packet, _) =
                encoder.encode(&src, w, h, crate::PixelFormat::Bgra).expect("encode");
            if !packet.is_empty() {
                packets.push(packet);
            }
        }
        drop(encoder);

        // `DB_LOOPS` replays the packets, so the decode runs long enough to show
        // up in a GPU performance counter sampled from outside.
        let loops: usize =
            std::env::var("DB_LOOPS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let wall = std::time::Instant::now();
        for _ in 0..loops {
            for packet in &packets {
                if decoder.decode(packet).expect("decode").is_some() {
                    decoded += 1;
                }
            }
        }
        let wall = wall.elapsed();

        // Sanity on the content itself: a synthetic pattern that changes every
        // pixel every frame is worst-case for H.264 and would make the decode
        // look slow for reasons no real desktop shares.
        let bytes: usize = packets.iter().map(|p| p.len()).sum();
        println!(
            "{} packets, {:.0} KB each, {:.1} Mbit/s at 60 fps",
            packets.len(),
            bytes as f64 / packets.len() as f64 / 1024.0,
            bytes as f64 / packets.len() as f64 * 8.0 * 60.0 / 1e6,
        );

        // The same packets through OpenH264, which is unambiguously software.
        // If Media Foundation is not clearly faster, it is not using the GPU.
        if let Ok(mut soft) = crate::codec_h264::H264Decoder::new() {
            let t = std::time::Instant::now();
            let mut got = 0u32;
            for packet in &packets {
                if soft.decode(packet).ok().flatten().is_some() {
                    got += 1;
                }
            }
            println!(
                "OpenH264 (known software): {:.2} ms/frame over {got} frames",
                t.elapsed().as_secs_f64() * 1000.0 / got.max(1) as f64,
            );
        }

        let n = decoded.max(1) as f64;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n;
        println!(
            "\n{decoded} frames at {w}x{h} ({}):\n  \
             wrap   {:.2} ms\n  input  {:.2} ms\n  output {:.2} ms\n  drain  {:.2} ms\n  \
             copy   {:.2} ms\n  map    {:.2} ms\n  rgba   {:.2} ms\n  TOTAL  {:.2} ms\
             \n  wall   {:.2} ms/frame = {:.0} fps\n",
            if decoder.gpu_convert { "GPU colour" } else { "CPU colour" },
            ms(decoder.stage_wrap),
            ms(decoder.stage_input),
            ms(decoder.stage_output),
            ms(decoder.stage_drain),
            ms(decoder.stage_copy),
            ms(decoder.stage_map),
            ms(decoder.stage_rgba),
            ms(decoder.stage_wrap
                + decoder.stage_input
                + decoder.stage_output
                + decoder.stage_drain
                + decoder.stage_copy
                + decoder.stage_map
                + decoder.stage_rgba),
            ms(wall),
            n / wall.as_secs_f64(),
        );
        assert!(decoded > 0, "nothing decoded across {FRAMES} frames");
    }
}
