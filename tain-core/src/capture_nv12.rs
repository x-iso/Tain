//! BGRA to NV12 on the GPU, for the capture path.
//!
//! ## Why this exists
//!
//! The desktop arrives as BGRA and the hardware encoder wants NV12, and doing
//! that conversion on the CPU costs twice over. Measured on this machine at
//! 2560x1600:
//!
//! | | |
//! |---|---|
//! | frame read back as BGRA | 15.62 MB |
//! | frame read back as NV12 | 5.86 MB |
//! | `bgra_to_nv12_into` on 16 threads | 3.48 ms — **52% of encode time** |
//!
//! Neither of those compresses anything. Converting on the GPU before the
//! readback removes the 3.48 ms outright and shrinks what crosses PCIe by 62%.
//!
//! ## Why a video processor and not a shader
//!
//! `ID3D11VideoProcessor` is fixed-function hardware on every GPU that can
//! decode video — which, on the range of machines this has to run on, is all of
//! them — and it does colour-space conversion and chroma subsampling in one
//! pass. A compute shader would work too, and would need to be written, tuned
//! and validated separately per vendor. This is the path the driver already
//! optimises for its own video pipeline.
//!
//! ## The colour space is not a detail
//!
//! [`super::codec_mf::bgra_to_nv12_into`] uses BT.601 limited range, and so
//! does the decoder on the other side. This converter must ask for exactly the
//! same thing (`DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601`) or the picture
//! shifts colour depending on which path happened to run — a bug that looks
//! like a bad monitor rather than a bad conversion.

use anyhow::{Context, Result};
use tracing::*;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorEnumerator1,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// The colour space the desktop is in: full-range sRGB.
const RGB_SPACE: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE =
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

/// The colour space the encoder is fed, matching the CPU converter exactly.
const YUV_SPACE: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE =
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601;

/// Converts a BGRA desktop texture into a packed NV12 buffer.
///
/// Owns the whole chain: the video processor, the NV12 render target it writes
/// to, and the staging texture the result is read back through.
pub struct Nv12Converter {
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    /// GPU-side NV12 target the blit writes into.
    target: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    /// CPU-readable copy of `target`.
    staging: ID3D11Texture2D,
    /// The input view, cached against the texture it was made from.
    ///
    /// Desktop Duplication usually hands back the same texture every frame, so
    /// this is built once. It is keyed on the raw pointer rather than assumed,
    /// because "usually" is not a guarantee and a view bound to the wrong
    /// texture would blit stale pixels.
    input_view: Option<(*mut core::ffi::c_void, ID3D11VideoProcessorInputView)>,
    width: u16,
    height: u16,
}

// SAFETY: every interface held here is `Send`/`Sync` in windows-rs, and the
// device this was built from is multithread-protected (see `Duplication`). The
// raw pointer in `input_view` is only ever compared, never dereferenced.
unsafe impl Send for Nv12Converter {}
unsafe impl Sync for Nv12Converter {}

impl Nv12Converter {
    /// Build a converter for frames of exactly this size.
    ///
    /// Fails — rather than falling back silently — if the driver cannot do this
    /// conversion. The caller keeps the CPU path in that case, which is correct
    /// but slower, and worth a line in the log.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        Self::scaling(device, context, width, height, width, height)
    }

    /// Build a converter that also **resizes**, from `src` to `dst`.
    ///
    /// The video processor rescales in the same pass it converts, for free. The
    /// CPU alternative — `box_downscale` over 4.1 million source pixels, single
    /// threaded, on the capture thread — is what the host was reporting as
    /// "Slow capture" every time the controller dropped below full resolution.
    /// Which is exactly when it is least able to afford it.
    pub fn scaling(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_width: u16,
        src_height: u16,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        // NV12 has a half-resolution chroma plane, so both axes must be even.
        // An odd size is not a driver problem to discover later.
        anyhow::ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "NV12 needs even dimensions, got {width}x{height}"
        );
        anyhow::ensure!(
            src_width > 0 && src_height > 0,
            "source is {src_width}x{src_height}"
        );

        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("this device has no video support — was VIDEO_SUPPORT requested?")?;
        let video: ID3D11VideoContext1 = context
            .cast()
            .context("device context is not a ID3D11VideoContext1 (needs D3D11.1)")?;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            // The rates only tell the driver what to optimise for; nothing here
            // is rate-controlled, and the blit is one frame in, one frame out.
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: src_width as u32,
            InputHeight: src_height as u32,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: width as u32,
            OutputHeight: height as u32,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        // SAFETY: desc is fully initialised and outlives the call.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("no video processor for this content description")?;

        // Ask before assuming. Every GPU that decodes video can do this, but
        // "every GPU" is a claim about hardware nobody here has tested on, and
        // the caller has a working CPU path to fall back to.
        //
        // The query itself is D3D11.1; a driver too old to answer it is not
        // evidence either way, so that case proceeds and lets the blit be the
        // judge rather than refusing on the strength of a missing interface.
        if let Ok(enumerator1) = enumerator.cast::<ID3D11VideoProcessorEnumerator1>() {
            // SAFETY: plain value arguments.
            let supported = unsafe {
                enumerator1.CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    RGB_SPACE,
                    DXGI_FORMAT_NV12,
                    YUV_SPACE,
                )
            }
            .context("could not query BGRA to NV12 support")?;
            anyhow::ensure!(supported.as_bool(), "this GPU will not convert BGRA to NV12");
        }

        // SAFETY: the enumerator outlives the processor it produces.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("could not create the video processor")?;

        // SAFETY: processor is live; both colour spaces are valid enum values.
        unsafe {
            video.VideoProcessorSetStreamColorSpace1(&processor, 0, RGB_SPACE);
            video.VideoProcessorSetOutputColorSpace1(&processor, YUV_SPACE);
            // No deinterlacing, no cadence detection — the desktop is progressive
            // and telling the driver so keeps it out of the reference-frame
            // machinery entirely.
            video.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
        }

        let target = create_texture(
            device,
            width,
            height,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0,
        )
        .context("could not create the NV12 render target")?;
        let staging = create_texture(
            device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )
        .context("could not create the NV12 staging texture")?;

        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut output_view = None;
        // SAFETY: target and enumerator outlive the view; out-param checked.
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &target,
                &enumerator,
                &view_desc,
                Some(&mut output_view),
            )
        }
        .context("could not create the NV12 output view")?;

        if (src_width, src_height) == (width, height) {
            info!("GPU colour conversion ready — BGRA to NV12 at {width}x{height}");
        } else {
            info!(
                "GPU colour conversion ready — BGRA {src_width}x{src_height} to NV12 {width}x{height}"
            );
        }
        Ok(Self {
            context: context.clone(),
            video_device,
            video,
            processor,
            enumerator,
            target,
            output_view: output_view.context("output view came back empty")?,
            staging,
            input_view: None,
            width,
            height,
        })
    }

    /// Packed NV12 bytes for one frame of this converter's size.
    pub fn output_len(&self) -> usize {
        nv12_len(self.width, self.height)
    }

    /// Convert one BGRA desktop texture and read the NV12 result back.
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let input = self.input_view_for(source)?;

        // Fill the whole target. Without this the driver is free to letterbox a
        // rescale, which shows up as a black border rather than an error.
        // SAFETY: processor is live; the rect covers the output texture exactly.
        unsafe {
            let dst = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            self.video.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
        }

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            pInputSurface: core::mem::ManuallyDrop::new(Some(input)),
            ..Default::default()
        };

        // SAFETY: the processor, output view and input surface are all live for
        // the duration of the call. `stream` holds a ManuallyDrop clone of the
        // input view, dropped explicitly below so the refcount balances.
        let blit = unsafe {
            self.video.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // The struct's `pInputSurface` is a ManuallyDrop, so nothing releases it
        // when `stream` goes out of scope. Leaking one reference per frame would
        // pin a texture forever.
        let mut stream = stream;
        // SAFETY: taken exactly once, and `stream` is not used again.
        unsafe { core::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        blit.context("VideoProcessorBlt failed")?;

        // SAFETY: staging matches target in format and size; unmapped below.
        unsafe {
            self.context.CopyResource(&self.staging, &self.target);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not map the NV12 staging texture")?;
            let out = read_nv12(&mapped, self.width, self.height);
            self.context.Unmap(&self.staging, 0);
            out
        }
    }

    /// The input view for this texture, building one if it is new.
    fn input_view_for(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = source.as_raw();
        if let Some((cached_key, view)) = &self.input_view
            && *cached_key == key
        {
            return Ok(view.clone());
        }

        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0, // 0 means "take the format from the resource".
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view = None;
        // SAFETY: source and enumerator outlive the call; out-param checked.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .context("could not create the video processor input view")?;

        let view = view.context("input view came back empty")?;
        self.input_view = Some((key, view.clone()));
        Ok(view)
    }
}

/// Packed NV12 size: a full luma plane plus a half-resolution interleaved one.
fn nv12_len(width: u16, height: u16) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + w * h.div_ceil(2)
}

/// Copy a mapped NV12 surface into a tightly packed buffer.
///
/// Two planes, one mapping. Y occupies `RowPitch * height` bytes and the
/// interleaved UV plane starts immediately after it, at half the row count.
/// `RowPitch` is almost never `width`, so both planes have to be unpadded row
/// by row — copying the mapping wholesale skews the luma and misplaces the
/// chroma, which shows up as a picture sheared into diagonal stripes.
///
/// # Safety
/// `mapped.pData` must point to at least `RowPitch * height * 3 / 2` readable bytes.
unsafe fn read_nv12(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u16,
    height: u16,
) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let pitch = mapped.RowPitch as usize;
    anyhow::ensure!(pitch >= w, "NV12 row pitch {pitch} is narrower than {w} pixels");
    anyhow::ensure!(!mapped.pData.is_null(), "NV12 mapping has no data");

    // `DepthPitch` is not a reliable size for a planar surface: this machine's
    // driver reports `RowPitch * height`, which is the luma plane alone, while
    // others report the whole thing. So it can only be checked as a lower bound
    // on the luma plane — the chroma offset below rests on the NV12 layout
    // convention (UV immediately follows Y at `RowPitch * height`), which is
    // what every implementation assumes and what this driver's own figure
    // happens to confirm.
    if mapped.DepthPitch != 0 {
        anyhow::ensure!(
            mapped.DepthPitch as usize >= pitch * h,
            "NV12 mapping is {} bytes, too small for a {w}x{h} luma plane",
            mapped.DepthPitch
        );
    }

    let mut out = vec![0u8; nv12_len(width, height)];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let base = mapped.pData as *const u8;

    for row in 0..h {
        // SAFETY: row < h, and the mapping holds at least `pitch * h` bytes.
        let src = unsafe { base.add(row * pitch) };
        let dst = &mut y_plane[row * w..(row + 1) * w];
        // SAFETY: exactly `w` bytes, inside both the mapping and the slice.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w) };
    }

    let chroma_rows = h.div_ceil(2);
    for row in 0..chroma_rows {
        // SAFETY: the UV plane follows the luma plane at `pitch * h`.
        let src = unsafe { base.add(pitch * h + row * pitch) };
        let dst = &mut uv_plane[row * w..(row + 1) * w];
        // SAFETY: as above.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w) };
    }

    Ok(out)
}

fn create_texture(
    device: &ID3D11Device,
    width: u16,
    height: u16,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind: u32,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width as u32,
        Height: height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: desc is fully initialised; the out-param is checked below.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D failed for NV12")?;
    texture.context("CreateTexture2D returned nothing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_size_matches_the_shared_definition() {
        // Two places compute this — here and in the encoder. They must agree, or
        // one of them writes past the end of the other's buffer.
        for (w, h) in [(64u16, 64u16), (2560, 1600), (1920, 1080), (2, 2)] {
            assert_eq!(nv12_len(w, h), super::super::codec_mf::nv12_len(w, h), "{w}x{h}");
        }
    }

    #[test]
    fn both_planes_are_unpadded_when_read_back() {
        // The driver pads rows. Copying the mapping wholesale shears the luma
        // and puts the chroma plane in the wrong place.
        let (w, h) = (4u16, 4u16);
        let pitch = 16usize; // deliberately wider than 4
        let rows = h as usize + h as usize / 2;
        let mut surface = vec![0u8; pitch * rows];
        for row in 0..rows {
            for x in 0..w as usize {
                surface[row * pitch + x] = (row * 16 + x) as u8;
            }
        }

        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: surface.as_mut_ptr() as *mut _,
            RowPitch: pitch as u32,
            DepthPitch: (pitch * rows) as u32,
        };
        // SAFETY: surface outlives the call and is large enough.
        let packed = unsafe { read_nv12(&mapped, w, h) }.expect("read");

        assert_eq!(packed.len(), nv12_len(w, h));
        // Luma: four rows of four, padding skipped.
        for row in 0..4 {
            let want = &surface[row * pitch..row * pitch + 4];
            assert_eq!(&packed[row * 4..row * 4 + 4], want, "luma row {row}");
        }
        // Chroma: two rows, starting after the *whole* luma plane in the source.
        for row in 0..2 {
            let src = (4 + row) * pitch;
            let dst = 16 + row * 4;
            assert_eq!(&packed[dst..dst + 4], &surface[src..src + 4], "chroma row {row}");
        }
    }

    #[test]
    fn a_mapping_that_is_too_small_is_refused_rather_than_read_past() {
        let (w, h) = (4u16, 4u16);
        let mut surface = vec![0u8; 8]; // nowhere near enough
        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: surface.as_mut_ptr() as *mut _,
            RowPitch: 4,
            DepthPitch: 8, // a 4x4 luma plane alone needs 16
        };
        // SAFETY: the call must bail on the size check before touching memory.
        assert!(unsafe { read_nv12(&mapped, w, h) }.is_err());
    }

    /// Colours the two paths must agree on, and what makes them disagree.
    ///
    /// Flat fields only — chroma subsampling averages across a 2x2 block, so a
    /// pattern with detail would differ for reasons that have nothing to do
    /// with the colour space. Saturated primaries are the useful cases: a
    /// BT.601/BT.709 mix-up barely moves grey but throws red and blue tens of
    /// levels apart, and a full/limited-range mix-up shows on black and white.
    const SWATCHES: [([u8; 3], &str); 7] = [
        ([128, 128, 128], "mid grey"),
        ([0, 0, 0], "black - catches a range mismatch"),
        ([255, 255, 255], "white - catches a range mismatch"),
        ([0, 0, 255], "red - catches a BT.601/709 mismatch"),
        ([0, 255, 0], "green"),
        ([255, 0, 0], "blue - catches a BT.601/709 mismatch"),
        ([200, 90, 40], "an arbitrary colour"),
    ];

    /// A D3D11 device with video support, or `None` if this machine has none.
    fn test_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        };

        let mut device = None;
        let mut context = None;
        // SAFETY: out-params are valid; failure leaves them None.
        let ok = unsafe {
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
        };
        if ok.is_err() {
            eprintln!("no D3D11 device with video support — skipping");
            return None;
        }
        Some((device?, context?))
    }

    /// A BGRA texture the video engine will accept as processor input.
    ///
    /// `DEFAULT`, not `IMMUTABLE`: an input view wants a texture the video
    /// engine can bind, and the duplication surface this stands in for is one.
    fn test_texture(
        device: &ID3D11Device,
        bgra: &[u8],
        w: u16,
        h: u16,
    ) -> ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
        };

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr() as *const _,
            SysMemPitch: w as u32 * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: desc and init describe `bgra` exactly.
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
            .expect("source texture");
        texture.expect("CreateTexture2D returned nothing")
    }

    /// A flat field of one colour, as BGRA.
    fn flat(bgr: [u8; 3], w: u16, h: u16) -> Vec<u8> {
        std::iter::repeat_n([bgr[0], bgr[1], bgr[2], 255], w as usize * h as usize)
            .flatten()
            .collect()
    }

    /// Rescaling in the same pass as the conversion.
    ///
    /// The alternative was `box_downscale` on the capture thread, which the
    /// host reported as "Slow capture" every time the controller dropped below
    /// full resolution. A flat field makes the check unambiguous: whatever the
    /// resampling does at the edges, the middle of a solid colour is that
    /// colour, and the output has to be the size that was asked for.
    #[test]
    #[ignore = "requires a D3D11 device with video support"]
    fn the_video_processor_resizes_while_it_converts() {
        let (sw, sh) = (256u16, 128u16);
        let (dw, dh) = (128u16, 64u16);
        let Some((device, context)) = test_device() else { return };

        let bgra: Vec<u8> = std::iter::repeat_n([40u8, 180, 90, 255], sw as usize * sh as usize)
            .flatten()
            .collect();
        let source = test_texture(&device, &bgra, sw, sh);

        let mut converter =
            Nv12Converter::scaling(&device, &context, sw, sh, dw, dh).expect("converter");
        let out = converter.convert(&source).expect("convert");

        assert_eq!(out.len(), nv12_len(dw, dh), "output must be the requested size");

        // Compare against converting the same colour without a resize: scaling
        // must not shift the colour, only the pixel count.
        let mut same_size = Nv12Converter::new(&device, &context, sw, sh).expect("converter");
        let reference = same_size.convert(&source).expect("convert");

        let mid = (dh as usize / 2) * dw as usize + dw as usize / 2;
        let (got, want) = (out[mid] as i32, reference[0] as i32);
        assert!(
            (got - want).abs() <= 3,
            "luma is {got} scaled and {want} unscaled — resizing changed the colour"
        );
    }

    /// The real thing, on whatever GPU is in this machine.
    #[test]
    #[ignore = "requires a D3D11 device with video support"]
    fn every_colour_converts_to_the_same_yuv_the_cpu_produces() {
        let (w, h) = (64u16, 64u16);
        let Some((device, context)) = test_device() else { return };

        let mut converter = Nv12Converter::new(&device, &context, w, h).expect("converter");
        let mut cpu = vec![0u8; nv12_len(w, h)];
        // A few levels: the fixed-function path rounds differently from the
        // scalar one. A colour-space or range mismatch is tens of levels, so
        // this separates the two without being brittle about the last bit.
        const TOLERANCE: i32 = 3;

        for (bgr, what) in SWATCHES {
            let bgra = flat(bgr, w, h);
            let source = test_texture(&device, &bgra, w, h);

            let gpu = converter.convert(&source).expect("convert");
            super::super::codec_mf::bgra_to_nv12_into(&bgra, w, h, &mut cpu);

            // A flat field means every sample in each plane is the same, so
            // comparing one of each compares all of them.
            let luma = w as usize * h as usize;
            for (offset, plane) in [(0usize, "luma"), (luma, "U"), (luma + 1, "V")] {
                let (g, c) = (gpu[offset] as i32, cpu[offset] as i32);
                assert!(
                    (g - c).abs() <= TOLERANCE,
                    "{what}: {plane} is {g} on the GPU and {c} on the CPU - \
                     that is a colour-space or range mismatch, not rounding"
                );
            }
        }
    }
}
