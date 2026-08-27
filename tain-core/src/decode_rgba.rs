//! NV12 to RGBA on the GPU, for the decode path.
//!
//! ## Why this exists
//!
//! The mirror image of [`super::capture_nv12`], and it exists for a measured
//! reason. Timing each stage inside the decoder separately gave, at 2038x1274
//! on a release build:
//!
//! | stage | ms/frame |
//! |---|---|
//! | `ProcessOutput` (the DXVA decode itself) | 10.5 |
//! | `CopySubresourceRegion` + `Flush` | 0.21 |
//! | `Map` | 0.55 |
//! | **`nv12_to_rgba` on the CPU** | **4.9** |
//!
//! Three rounds of reasoning had blamed the readback. The readback is 0.77 ms.
//! The colour conversion is nearly seven times that, and it is pure arithmetic
//! over 2.6 million pixels on the thread that also has to drive the window.
//!
//! Doing it in the same fixed-function block that already did the forward
//! conversion removes it. The readback grows — RGBA is twice the bytes of NV12 —
//! but 0.77 ms of transfer buys back 4.9 ms of CPU.
//!
//! ## Why RGBA and not BGRA
//!
//! `DXGI_FORMAT_R8G8B8A8_UNORM` lands in memory as R, G, B, A, which is exactly
//! `egui::Color32`. Asking the video processor for the other byte order would
//! mean swizzling 10 MB per frame afterwards, which is the CPU pass this module
//! is here to delete.
//!
//! ## What is different from the capture side
//!
//! A decoder does not hand back a lone texture. Its output surfaces are slices
//! of a decode picture buffer — one `ID3D11Texture2D` with an `ArraySize` of
//! however many reference frames the stream needs — and it rotates through them.
//! So the input view has to name a slice, and caching one view against one
//! pointer would blit whichever frame happened to be in slice zero.
//!
//! ## The colour space is not a detail
//!
//! The encoder wrote BT.601 limited range, so that is what has to be read back
//! out. Getting this wrong does not fail; it washes the picture out, which looks
//! like a bad capture rather than a bad conversion.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::*;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorEnumerator1,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE,
    ID3D11Query, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_TEX2D_VPIV,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// What the decoder produces, matching what the encoder was fed.
const YUV_SPACE: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE =
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601;

/// What the window draws: full-range sRGB.
const RGB_SPACE: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE =
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

/// How many input views to keep before starting over.
///
/// One per surface in the decoder's picture buffer, which for the profiles this
/// streams is well under this. The bound exists so that a stream that somehow
/// keeps producing fresh textures leaks nothing worse than a rebuild.
const VIEW_CACHE: usize = 32;

/// RGBA targets kept in rotation. See the field's own note for why three.
const TARGETS: usize = 3;

/// One RGBA surface the video processor can render into.
struct Target {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
}

/// Converts a decoded NV12 surface into RGBA the window can upload directly.
///
/// Owns the video processor, the RGBA texture it renders into, and the staging
/// texture the result comes back through.
pub struct RgbaConverter {
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    /// GPU-side RGBA targets the blit writes into, used in rotation.
    ///
    /// # Why three, and what that does and does not guarantee
    ///
    /// The picture is now drawn from one of these directly rather than read
    /// back, so the decoder must not write the one being drawn. The render
    /// thread always holds the most recently published target, and the next blit
    /// goes to the one after it — so with three, the decoder has to get two
    /// whole frames ahead before it can come back around to a target the render
    /// thread is still holding.
    ///
    /// That is a bound, not a proof. A render thread that stalls long enough to
    /// miss three published frames while still drawing the stale handle would
    /// see a torn frame. Making it airtight needs the render thread to publish
    /// which target it holds, and the failure it would prevent is one torn
    /// frame under a stall that is already visible as a stall — so it is written
    /// down here rather than built.
    targets: Vec<Target>,
    /// The target the next blit goes into.
    next: usize,
    /// The target the consumer says it is drawing, as a texture address.
    ///
    /// # Why three targets was not enough on its own
    ///
    /// The rotation was chosen so the decoder has to get two whole frames ahead
    /// before it can come back around to a target the render thread is still
    /// holding, and the note on `targets` admits that this is a bound rather
    /// than a proof: a render thread that misses three published frames while
    /// still drawing the stale handle sees the surface change under it.
    ///
    /// It is not a rare case. The render thread holds the last picture it was
    /// given until a newer one *and a repaint* arrive, and it draws that same
    /// picture on every repaint in between. Any stretch where the decoder is
    /// ahead of the window — a busy compositor, a frame the window missed, a
    /// stream running faster than the display — laps it, and what that looks
    /// like is a trail of previous frames rather than an error.
    ///
    /// So the render thread says which one it has, and that one is skipped.
    /// Zero means it is drawing none of them.
    in_use: usize,
    /// The target the last blit went into, which is what `read` picks up.
    last: usize,
    /// CPU-readable copy of `target`.
    staging: ID3D11Texture2D,
    /// The surface size the processor was described with.
    ///
    /// Starts as the picture size and is corrected the first time a decoder
    /// hands back something larger — see [`build_processor`] for why describing
    /// it wrongly turns the picture green rather than merely cropping it.
    source: (u16, u16),
    /// Signals when the GPU has actually finished the last blit.
    ///
    /// # Why submitting is not the same as finishing
    ///
    /// [`Self::submit`] queues the blit and flushes, which starts it — it does
    /// not wait for it. That is fine for [`Self::read`], where mapping the
    /// staging texture waits on the CPU's behalf as a side effect.
    ///
    /// It is not fine for the zero-copy path. There the target is handed to
    /// OpenGL and sampled on the render thread, and `WGL_NV_DX_interop` is
    /// explicit that the application must ensure Direct3D has finished with a
    /// resource before locking it. Nothing here was doing that, so the render
    /// thread could sample a target the GPU had not written yet and get
    /// whatever was in it before — which, with three targets in rotation, is
    /// the frame from three frames ago.
    ///
    /// That is the trail of repeated frames reported from an NVIDIA guest once
    /// the surfaces were shareable and the picture stopped freezing outright.
    fence: ID3D11Query,
    /// Views cached against the (texture, array slice) they were built from.
    ///
    /// Keyed on both because a decoder rotates through slices of one texture:
    /// the pointer alone is the same for every frame in flight, and a view
    /// matched on it would keep blitting whatever landed in slice zero.
    views: Vec<(*mut core::ffi::c_void, u32, ID3D11VideoProcessorInputView)>,
    width: u16,
    height: u16,
}

// SAFETY: every interface held here is `Send`/`Sync` in windows-rs, and the
// device is multithread-protected where it is created. The raw pointers in
// `views` are only ever compared, never dereferenced.
unsafe impl Send for RgbaConverter {}
unsafe impl Sync for RgbaConverter {}

impl RgbaConverter {
    /// Build a converter for decoded frames of exactly this size.
    ///
    /// Fails rather than falling back silently: the caller keeps a working CPU
    /// path, and a driver that cannot do this is worth a line in the log.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        anyhow::ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "NV12 needs even dimensions, got {width}x{height}"
        );

        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("this device has no video support — was VIDEO_SUPPORT requested?")?;
        let video: ID3D11VideoContext1 = context
            .cast()
            .context("device context is not a ID3D11VideoContext1 (needs D3D11.1)")?;

        // Built for a source the size of the picture, which is a guess: a
        // decoder is free to hand back something larger. `submit` corrects it
        // the moment it sees the real surface — see `build_processor`.
        let (enumerator, processor) =
            build_processor(&video_device, &video, (width, height), (width, height))?;

        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut targets = Vec::with_capacity(TARGETS);
        for _ in 0..TARGETS {
            // SHADER_RESOURCE as well as RENDER_TARGET: the zero-copy path
            // hands this texture to OpenGL, and a surface GL is going to
            // sample from has to be created able to be sampled.
            let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            // # Why these are shared and the staging texture is not
            //
            // `WGL_NV_DX_interop2` is registering these, and a resource handed
            // across an API boundary that way is expected to have been created
            // shareable. AMD does not insist; NVIDIA does, and not by failing —
            // registration succeeds, the lock succeeds, and the surface GL sees
            // simply never changes again.
            //
            // That is the frozen picture reported from an RTX guest, and it
            // took the registration count to find: exactly three registrations
            // in one generation and then silence, which says the addresses were
            // cycling correctly and being found in the cache, so the only thing
            // left was the contents.
            //
            // Shared is not free — it can be refused, and there is no reason to
            // ask for it on a texture nobody outside this device will ever see.
            // The staging texture is read back by the CPU here and nowhere
            // else, and STAGING cannot be shared in any case.
            let texture = create_rgba(
                device,
                width,
                height,
                D3D11_USAGE_DEFAULT,
                bind,
                0,
                D3D11_RESOURCE_MISC_SHARED.0 as u32,
            )
            // A driver that will not share leaves the readback path, which
            // wants nothing from this flag and is what runs if the interop
            // gives up anyway. Better a slower picture than none.
            .or_else(|e| {
                debug!("No shared RGBA target ({e:#}) — the GPU path may not work here");
                create_rgba(device, width, height, D3D11_USAGE_DEFAULT, bind, 0, 0)
            })
            .context("could not create an RGBA render target")?;

            let mut output_view = None;
            // SAFETY: texture and enumerator outlive the view; out-param checked.
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &view_desc,
                    Some(&mut output_view),
                )
            }
            .context("could not create the RGBA output view")?;
            targets.push(Target {
                texture,
                view: output_view.context("output view came back empty")?,
            });
        }

        let staging = create_rgba(
            device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
            0,
        )
        .context("could not create the RGBA staging texture")?;

        let fence_desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
        let mut fence = None;
        // SAFETY: the descriptor is a plain value; the out-param is checked.
        unsafe { device.CreateQuery(&fence_desc, Some(&mut fence)) }
            .context("could not create the blit-completion query")?;

        info!("GPU colour conversion ready — NV12 to RGBA at {width}x{height}");
        Ok(Self {
            context: context.clone(),
            video_device,
            video,
            processor,
            enumerator,
            targets,
            next: 0,
            in_use: 0,
            last: 0,
            staging,
            source: (width, height),
            fence: fence.context("query came back empty")?,
            views: Vec::new(),
            width,
            height,
        })
    }

    /// RGBA bytes for one frame of this converter's size.
    pub fn output_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Say which target the consumer is drawing, so the next blit avoids it.
    ///
    /// `0` for none. See [`Self::in_use`] for why this is needed and what goes
    /// wrong without it.
    pub fn set_in_use(&mut self, texture: usize) {
        self.in_use = texture;
    }

    /// A reference to the texture holding the frame `submit` last produced.
    ///
    /// A reference rather than a pointer, because whoever draws this is on
    /// another thread and outlives nothing in particular: the session can end,
    /// and this converter be dropped, while a render thread is still holding
    /// the last frame it was handed. A COM reference costs an increment and
    /// makes that safe; an address does not, and the crash it produces is a
    /// silent process exit rather than anything that names itself.
    pub fn last_texture(&self) -> ID3D11Texture2D {
        self.targets[self.last].texture.clone()
    }

    /// Queue the conversion of one decoded surface, without waiting for it.
    ///
    /// `slice` is the array index the decoder reported for this frame. The blit
    /// and the staging copy are both submitted and left to run; [`Self::read`]
    /// picks the result up on the next call, by which point the GPU has had a
    /// whole frame period to finish.
    pub fn submit(&mut self, source: &ID3D11Texture2D, slice: u32) -> Result<()> {
        // # What the surface actually is, rather than what it should be
        //
        // This converter is built for NV12 and nothing checked. A decoder handed
        // a ten-bit stream returns P010 instead, and P010 read as NV12 does not
        // fail — it produces a dark, broken picture, which is a far worse
        // outcome than an error naming the format. That cost a round of "the
        // colour profile seems wrong" before anyone looked here.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: reading the description of a texture we were handed.
        unsafe { source.GetDesc(&mut desc) };
        anyhow::ensure!(
            desc.Format == DXGI_FORMAT_NV12,
            "decoded surface is {:?}, not NV12 — this stream is not 8-bit 4:2:0",
            desc.Format
        );

        // The surface the decoder actually produced, which is not necessarily
        // the picture inside it. Rebuilding on a change rather than assuming at
        // construction, because nothing knows the answer until a frame arrives
        // and different codecs give different ones.
        let arrived = (desc.Width as u16, desc.Height as u16);
        if arrived != self.source {
            info!(
                "Decoded surface is {}x{} for a {}x{} picture — describing the conversion \
                 to match",
                arrived.0, arrived.1, self.width, self.height
            );
            let (enumerator, processor) = build_processor(
                &self.video_device,
                &self.video,
                arrived,
                (self.width, self.height),
            )?;
            self.enumerator = enumerator;
            self.processor = processor;
            self.source = arrived;
            // Every view was made against the enumerator just replaced, and a
            // view belongs to the enumerator it came from. The inputs are a
            // cache and can simply go; the outputs are the targets themselves
            // and have to be made again, or the blit writes through a view the
            // new processor does not own.
            self.views.clear();
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            for target in &mut self.targets {
                let mut view = None;
                // SAFETY: the texture and the new enumerator both outlive the
                // view; the out-parameter is checked.
                unsafe {
                    self.video_device.CreateVideoProcessorOutputView(
                        &target.texture,
                        &self.enumerator,
                        &view_desc,
                        Some(&mut view),
                    )
                }
                .context("could not rebuild an RGBA output view")?;
                target.view = view.context("output view came back empty")?;
            }
        }

        let input = self.input_view_for(source, slice)?;

        // # Both rects, and why the source one is not optional
        //
        // The destination rect fills the whole target — without it the driver
        // may letterbox, which shows up as a black border rather than an error.
        //
        // The **source** rect is the one that was missing, and it is the reason
        // a degraded stream grew a green stripe down its edge. A decoder does
        // not hand back a surface the size of the picture: H.264 codes in 16x16
        // macroblocks, so a 1140x642 frame lives in an 1152x656 texture and the
        // real picture is the top-left corner of it. With no source rect the
        // processor takes the *whole texture*, so the padding — which the
        // decoder never writes, and which is therefore zeroed — is scaled into
        // the picture. Zero in NV12 is not black, it is (0, 135, 0): the exact
        // green in the report. The rest of the frame is squashed slightly to
        // make room for it.
        //
        // It went unseen for so long because it only happens at sizes that are
        // not multiples of 16, and the display everything was tested on is
        // 2560x1600 — both exact multiples. It appears the moment the adaptive
        // controller scales down, which is to say only ever on a bad link, which
        // is where nobody could tell it apart from the bad link.
        //
        // SAFETY: processor is live; both rects are within the surfaces.
        unsafe {
            let picture = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            self.video.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&picture),
            );
            self.video.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&picture),
            );
        }

        // Step over the target the consumer says it is drawing. Bounded by the
        // number of targets, so a consumer naming something that is not one of
        // them — or naming the only one left — cannot spin here.
        for _ in 0..TARGETS {
            use windows::core::Interface;
            if self.targets[self.next].texture.as_raw() as usize != self.in_use {
                break;
            }
            self.next = (self.next + 1) % TARGETS;
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

        // SAFETY: processor, output view and input surface are live for the
        // call. `stream` holds a ManuallyDrop clone of the input view, dropped
        // explicitly below so the refcount balances.
        let blit = unsafe {
            self.video.VideoProcessorBlt(
                &self.processor,
                &self.targets[self.next].view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        let mut stream = stream;
        // SAFETY: taken exactly once, and `stream` is not used again.
        unsafe { core::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        blit.context("VideoProcessorBlt failed")?;

        // Rotate. `last` is what a reader or a draw refers to; `next` moves on
        // so the following blit cannot land on it.
        self.last = self.next;
        self.next = (self.next + 1) % TARGETS;

        // SAFETY: the blit is queued; ending the query marks the point after it
        // and flushing starts both. Neither waits — see `finish`, which does.
        //
        // The staging copy that used to be here has moved into `read`, which is
        // the only thing that needs it. The zero-copy path never calls `read`,
        // and doing the copy here would have made it pay for a readback it does
        // not perform.
        unsafe {
            self.context.End(&self.fence);
            self.context.Flush();
        }
        Ok(())
    }

    /// Wait until the GPU has actually finished the last [`Self::submit`].
    ///
    /// Only the zero-copy path needs this, and it needs it before the target is
    /// handed to anyone else: OpenGL samples the surface directly, so reading it
    /// while the blit is still in flight shows whatever the target held before —
    /// three frames ago, given the rotation. [`Self::read`] does not call this
    /// because mapping the staging texture already waits.
    ///
    /// The wait is for one blit at desktop size, which is a fraction of a
    /// millisecond, against a frame interval of twenty-odd. It is bounded
    /// anyway: a GPU that has not finished in `limit` is in trouble no decoder
    /// can help with, and returning is better than never drawing again.
    pub fn finish(&self, limit: Duration) -> bool {
        let started = std::time::Instant::now();
        loop {
            let mut done = windows::Win32::Foundation::BOOL(0);
            // SAFETY: the query is live and `done` is a BOOL of the size given.
            // An unfinished query returns S_FALSE and writes nothing, which is
            // why `done` starts false and is not trusted from the return value —
            // windows-rs reports S_FALSE as success.
            let _ = unsafe {
                self.context.GetData(
                    &self.fence,
                    Some(std::ptr::from_mut(&mut done).cast()),
                    std::mem::size_of::<windows::Win32::Foundation::BOOL>() as u32,
                    0,
                )
            };
            if done.as_bool() {
                return true;
            }
            if started.elapsed() >= limit {
                debug!("The colour conversion did not finish within {limit:?}");
                return false;
            }
            std::thread::yield_now();
        }
    }

    /// Read whatever [`Self::submit`] last put in the staging texture.
    ///
    /// Resizes `out` to a full frame and fills it. Rows are copied one at a
    /// time because a staging texture's pitch is whatever the driver chose and
    /// is essentially never the width.
    pub fn read(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        out.resize(w * h * 4, 0);

        // SAFETY: staging matches the target in format and size.
        unsafe {
            self.context.CopyResource(&self.staging, &self.targets[self.last].texture);
        }

        // SAFETY: mapping a staging texture we own; unmapped on both paths.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("could not map the decoded RGBA staging texture")?;
            let result = copy_rows(&mapped, w, h, out);
            self.context.Unmap(&self.staging, 0);
            result
        }
    }

    /// An input view for this surface and slice, built once and kept.
    fn input_view_for(
        &mut self,
        source: &ID3D11Texture2D,
        slice: u32,
    ) -> Result<ID3D11VideoProcessorInputView> {
        let key = source.as_raw();
        if let Some((_, _, view)) =
            self.views.iter().find(|(k, s, _)| *k == key && *s == slice)
        {
            return Ok(view.clone());
        }

        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0, // 0 means "take the format from the resource".
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
            },
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
        if self.views.len() >= VIEW_CACHE {
            self.views.clear();
        }
        self.views.push((key, slice, view.clone()));
        Ok(view)
    }
}

/// Copy a mapped RGBA surface into a tightly packed buffer.
fn copy_rows(mapped: &D3D11_MAPPED_SUBRESOURCE, w: usize, h: usize, out: &mut [u8]) -> Result<()> {
    let pitch = mapped.RowPitch as usize;
    let row = w * 4;
    anyhow::ensure!(pitch >= row, "RGBA row pitch {pitch} is under {row} bytes");
    anyhow::ensure!(out.len() >= row * h, "destination is {} bytes", out.len());

    for y in 0..h {
        // SAFETY: the map covers `pitch * h` bytes and this reads `row` of them
        // starting at `pitch * y`, which is inside that for every y below h.
        let src = unsafe { (mapped.pData as *const u8).add(pitch * y) };
        // SAFETY: `out` is at least `row * h`, checked above; the ranges are
        // disjoint because `out` is a Rust buffer and `pData` is the mapping.
        unsafe { std::ptr::copy_nonoverlapping(src, out[row * y..].as_mut_ptr(), row) };
    }
    Ok(())
}

/// Build a video processor that converts `source` NV12 into `output` RGBA.
///
/// # Why the source size is not the picture size
///
/// A hardware decoder returns the picture inside whatever surface its codec
/// wants. H.264 hands back exactly the picture; HEVC pads the height up to its
/// coding-tree size, so a 640x480 picture arrives in a 640x512 surface.
///
/// In NV12 the chroma plane begins after the luma one, so where it begins
/// depends on the height of the *surface*. Describing the input as 640x480 when
/// it is really 640x512 has the driver look for chroma thirty-two rows early —
/// inside the luma padding, which is zeros. Zero chroma is not a subtle shift:
/// it is a flat green field with the luma showing through as tinted shapes,
/// which is exactly what an HEVC session looked like, on every frame, while
/// H.264 through the identical code was perfect.
///
/// The source rect set per frame is what crops the padding back off, so the
/// output stays the picture and only the description changes.
fn build_processor(
    video_device: &ID3D11VideoDevice,
    video: &ID3D11VideoContext1,
    source: (u16, u16),
    output: (u16, u16),
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        InputWidth: source.0 as u32,
        InputHeight: source.1 as u32,
        OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
        OutputWidth: output.0 as u32,
        OutputHeight: output.1 as u32,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };

    // SAFETY: desc is fully initialised and outlives the call.
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
        .context("no video processor for this content description")?;

    // Ask before assuming, as on the capture side. A driver too old to
    // answer the D3D11.1 query is not evidence either way, so that case
    // proceeds and lets the blit be the judge.
    if let Ok(enumerator1) = enumerator.cast::<ID3D11VideoProcessorEnumerator1>() {
        // SAFETY: plain value arguments.
        let supported = unsafe {
            enumerator1.CheckVideoProcessorFormatConversion(
                DXGI_FORMAT_NV12,
                YUV_SPACE,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                RGB_SPACE,
            )
        }
        .context("could not query NV12 to RGBA support")?;
        anyhow::ensure!(supported.as_bool(), "this GPU will not convert NV12 to RGBA");
    }

    // SAFETY: the enumerator outlives the processor it produces.
    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
        .context("could not create the video processor")?;

    // SAFETY: processor is live; both colour spaces are valid enum values.
    unsafe {
        video.VideoProcessorSetStreamColorSpace1(&processor, 0, YUV_SPACE);
        video.VideoProcessorSetOutputColorSpace1(&processor, RGB_SPACE);
        video.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
    }

    Ok((enumerator, processor))
}

fn create_rgba(
    device: &ID3D11Device,
    width: u16,
    height: u16,
    usage: D3D11_USAGE,
    bind: u32,
    cpu_access: u32,
    misc: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width as u32,
        Height: height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        MiscFlags: misc,
    };
    let mut texture = None;
    // SAFETY: desc is fully initialised; the out-param is checked below.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("CreateTexture2D failed for RGBA")?;
    texture.context("CreateTexture2D returned nothing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    };

    /// The surface being drawn must never be the one written next.
    ///
    /// # The trail this is about
    ///
    /// Three targets in rotation means the decoder comes back around to a given
    /// one every third frame. The render thread holds the last picture it was
    /// handed and redraws it on every repaint until a newer one arrives, so any
    /// stretch where the decoder is ahead of the window ends with the decoder
    /// writing the surface the window is reading — which is not an error, it is
    /// a picture made partly of an older frame. Reported as a trail of previous
    /// frames, almost all the time.
    ///
    /// Asserted against the address the render thread would actually publish,
    /// over enough frames that the rotation wraps several times. Without the
    /// skip it fails on the third blit.
    #[test]
    fn the_target_being_drawn_is_never_the_one_written_next() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        let (w, h) = (640u16, 360u16);
        let Ok(mut converter) = RgbaConverter::new(&device, &context, w, h) else {
            eprintln!("no GPU colour conversion here — skipping");
            return;
        };
        let source = flat_nv12(&device, w, h, 126, 128, 128).expect("source");

        // Take a picture and hold it, exactly as the render thread does.
        converter.submit(&source, 0).expect("submit");
        let held = {
            use windows::core::Interface;
            converter.last_texture().as_raw() as usize
        };
        converter.set_in_use(held);

        // Now let the decoder run on without the window ever moving.
        for frame in 0..(TARGETS * 4) {
            converter.submit(&source, 0).expect("submit");
            let wrote = {
                use windows::core::Interface;
                converter.last_texture().as_raw() as usize
            };
            assert_ne!(
                wrote, held,
                "frame {frame} was blitted into the surface the window is drawing, which is \
                 a picture made half of this frame and half of the last one"
            );
        }

        // And releasing it lets the rotation use every target again.
        converter.set_in_use(0);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..(TARGETS * 2) {
            converter.submit(&source, 0).expect("submit");
            use windows::core::Interface;
            seen.insert(converter.last_texture().as_raw() as usize);
        }
        assert_eq!(seen.len(), TARGETS, "the rotation did not come back to every target");
    }

    /// The blit has to be reported finished, and reported so promptly.
    ///
    /// The zero-copy path hands the target straight to OpenGL, which samples it
    /// on another thread, so it must not be handed over while the GPU is still
    /// writing it — that shows whatever the target held before, which with three
    /// in rotation is the frame from three ago. `submit` only starts the blit;
    /// `finish` is what waits.
    ///
    /// This cannot prove the render thread sees the right pixels — any CPU-side
    /// check of that would itself wait, and so would pass whether or not
    /// `finish` did anything. Nor can it tell a query ended after the blit from
    /// one ended before it, since both signal.
    ///
    /// What it does catch is the query not signalling at all: the wrong kind of
    /// query, or one never ended, both of which leave `finish` spending its
    /// whole timeout and returning false while the picture goes on trailing.
    /// Dropping the `End` call fails it in 2.2 seconds.
    #[test]
    fn a_submitted_conversion_reports_itself_finished() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        let (w, h) = (640u16, 360u16);
        let Ok(mut converter) = RgbaConverter::new(&device, &context, w, h) else {
            eprintln!("no NV12 to RGBA conversion on this GPU — skipping");
            return;
        };

        let source = flat_nv12(&device, w, h, 126, 128, 128).expect("source");
        converter.submit(&source, 0).expect("submit");

        let started = std::time::Instant::now();
        assert!(
            converter.finish(Duration::from_secs(2)),
            "the blit never reported itself finished, so nothing downstream can know \
             when the target is safe to read"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "waiting for one blit took {:?}, which is not a wait a frame can afford",
            started.elapsed()
        );

        // And it must still be true a second time round, since the query is
        // reused for every frame rather than made afresh.
        converter.submit(&source, 0).expect("submit");
        assert!(converter.finish(Duration::from_secs(2)));
    }

    /// Every target the converter draws into must be shareable.
    ///
    /// `WGL_NV_DX_interop2` registers these, and a resource crossing to another
    /// API that way is expected to have been created shareable. NVIDIA enforces
    /// it in the worst possible way — registration succeeds, the lock succeeds,
    /// and the surface GL sees never changes again — so nothing anywhere else
    /// in this codebase would catch its absence. A guest reported a frozen
    /// picture and the log showed a perfectly healthy stream behind it.
    ///
    /// Asserted on the texture rather than on the descriptor that was passed
    /// in, because a driver is free to refuse the flag, and there is a fallback
    /// here that quietly builds an unshared target when it does. If this fails
    /// on some machine, that machine cannot draw without a readback and wants
    /// to know rather than to freeze.
    #[test]
    fn the_targets_handed_to_opengl_are_shareable() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        let Ok(converter) = RgbaConverter::new(&device, &context, 640, 360) else {
            eprintln!("no GPU colour conversion here — skipping");
            return;
        };

        for (index, target) in converter.targets.iter().enumerate() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: reading the description of a texture this owns.
            unsafe { target.texture.GetDesc(&mut desc) };
            assert!(
                desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED.0 as u32 != 0,
                "target {index} was created unshared, which on an NVIDIA guest is a \
                 picture that freezes while every log line says the stream is fine"
            );
        }
    }

    /// A device with video support, or `None` on a machine without one.
    fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut context = None;
        // SAFETY: out-params are valid; failure leaves both None.
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
        .ok()?;
        Some((device?, context?))
    }

    /// An NV12 texture holding one flat colour, as the decoder would produce.
    fn flat_nv12(
        device: &ID3D11Device,
        w: u16,
        h: u16,
        y: u8,
        u: u8,
        v: u8,
    ) -> Result<ID3D11Texture2D> {
        let (wu, hu) = (w as usize, h as usize);
        let mut bytes = vec![y; wu * hu];
        for _ in 0..(wu * hu / 4) {
            bytes.push(u);
            bytes.push(v);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // A video processor input view refuses a texture the GPU cannot
            // render to — E_INVALIDARG, with nothing to say which argument.
            // Real decoder surfaces carry `BIND_DECODER` and satisfy this on
            // their own; a hand-made source has to ask.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr() as *const _,
            SysMemPitch: w as u32,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: `bytes` covers a full NV12 frame at this pitch and outlives
        // the call, which is all CreateTexture2D reads.
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
            .context("could not create the source NV12 texture")?;
        texture.context("CreateTexture2D returned nothing")
    }

    /// A decoder surface: mid grey inside the picture, untouched outside it.
    ///
    /// The padding is left at zero deliberately. That is what a decoder leaves
    /// there, and zero in NV12 converts to a strong green rather than to black —
    /// which is what makes folding the padding into the picture so visible.
    fn nv12_with_padding(
        device: &ID3D11Device,
        surface_w: u16,
        surface_h: u16,
        picture_w: u16,
        picture_h: u16,
    ) -> Result<ID3D11Texture2D> {
        let (sw, sh) = (surface_w as usize, surface_h as usize);
        let (pw, ph) = (picture_w as usize, picture_h as usize);

        let mut bytes = vec![0u8; sw * sh + sw * sh / 2];
        for y in 0..ph {
            bytes[y * sw..y * sw + pw].fill(126);
        }
        // Chroma is half resolution in both directions, two bytes per sample.
        let uv = sw * sh;
        for y in 0..ph / 2 {
            for x in 0..pw / 2 {
                bytes[uv + y * sw + x * 2] = 128;
                bytes[uv + y * sw + x * 2 + 1] = 128;
            }
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: surface_w as u32,
            Height: surface_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr() as *const _,
            SysMemPitch: surface_w as u32,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: `bytes` covers a full NV12 surface at this pitch and outlives
        // the call, which is all CreateTexture2D reads.
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
            .context("could not create the padded NV12 source")?;
        texture.context("CreateTexture2D returned nothing")
    }

    /// A picture smaller than the surface it arrived in must come back whole.
    ///
    /// # The bug this pins
    ///
    /// A decoder pads to whole macroblocks, so a 1140x642 picture is delivered
    /// in an 1152x656 texture with the real content in the top-left corner. With
    /// no source rect the video processor consumed the entire texture, scaling
    /// the never-written padding into the frame — and zeroed NV12 is green, not
    /// black. That is the stripe down the edge of a degraded stream.
    ///
    /// Every earlier test built a source texture at exactly the converter's
    /// size, so none of them could see it. This one deliberately does not.
    #[test]
    fn a_picture_padded_out_to_macroblocks_is_not_stretched_or_fringed() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        // A picture size that is not a multiple of 16, in the surface a decoder
        // would actually hand back for it.
        let (picture_w, picture_h) = (40u16, 24u16);
        let (surface_w, surface_h) = (48u16, 32u16);

        let Ok(mut converter) = RgbaConverter::new(&device, &context, picture_w, picture_h) else {
            eprintln!("no NV12 to RGBA conversion on this GPU — skipping");
            return;
        };

        // Grey inside the picture, zeroes outside it — which is what a decoder
        // actually leaves in the padding, and what turns green on conversion.
        // A flat surface would not do: with everything the same colour, folding
        // the padding in is invisible and this test would pass either way.
        let source =
            nv12_with_padding(&device, surface_w, surface_h, picture_w, picture_h).expect("source");
        converter.submit(&source, 0).expect("submit");
        let mut out = Vec::new();
        converter.read(&mut out).expect("read");

        assert_eq!(out.len(), picture_w as usize * picture_h as usize * 4);
        // Grey everywhere, including the last column and row — which is where
        // the padding used to be folded in.
        for (index, px) in out.chunks_exact(4).enumerate() {
            let (x, y) = (index % picture_w as usize, index / picture_w as usize);
            for (channel, got) in px[..3].iter().enumerate() {
                assert!(
                    got.abs_diff(126) <= 3,
                    "pixel ({x},{y}) channel {channel} is {got}, not the grey that was sent"
                );
            }
        }
    }

    /// The conversion has to agree with the CPU path it replaces, or the
    /// picture changes colour depending on which one happened to run.
    #[test]
    fn flat_colours_come_back_as_the_cpu_would_have_produced() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        let (w, h) = (64u16, 64u16);
        let Ok(mut converter) = RgbaConverter::new(&device, &context, w, h) else {
            eprintln!("no NV12 to RGBA conversion on this GPU — skipping");
            return;
        };

        // BT.601 limited range, the same swatches the forward converter is
        // tested against: black, white, and a mid grey.
        for (y, u, v, expect) in
            [(16u8, 128u8, 128u8, 0u8), (235, 128, 128, 255), (126, 128, 128, 126)]
        {
            let source = flat_nv12(&device, w, h, y, u, v).expect("source texture");
            converter.submit(&source, 0).expect("submit");
            let mut out = Vec::new();
            converter.read(&mut out).expect("read");

            assert_eq!(out.len(), w as usize * h as usize * 4);
            // Sample the middle, away from any edge filtering.
            let i = ((h as usize / 2) * w as usize + w as usize / 2) * 4;
            for (channel, got) in out[i..i + 3].iter().enumerate() {
                let delta = got.abs_diff(expect);
                assert!(
                    delta <= 4,
                    "channel {channel} of Y={y} came back {got}, expected about {expect}"
                );
            }
            assert_eq!(out[i + 3], 255, "alpha must be opaque");
        }
    }

    #[test]
    fn odd_dimensions_are_refused_rather_than_discovered_later() {
        let Some((device, context)) = device() else {
            eprintln!("no D3D11 device with video support — skipping");
            return;
        };
        assert!(RgbaConverter::new(&device, &context, 65, 64).is_err());
        assert!(RgbaConverter::new(&device, &context, 64, 65).is_err());
    }

    #[test]
    fn a_padded_row_pitch_is_unpacked_rather_than_copied_straight_through() {
        // The driver pads staging rows; copying the mapping wholesale would
        // shear the picture. Two rows of two pixels at a four-pixel pitch.
        let src: Vec<u8> = (0u8..32).collect();
        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: src.as_ptr() as *mut _,
            RowPitch: 16,
            DepthPitch: 32,
        };
        let mut out = vec![0u8; 2 * 2 * 4];
        copy_rows(&mapped, 2, 2, &mut out).expect("copy");
        assert_eq!(out[..8], src[..8], "first row");
        assert_eq!(out[8..], src[16..24], "second row skips the padding");
    }
}
