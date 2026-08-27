//! Hardware H.264 decode through libavcodec's D3D11VA path.
//!
//! ## Why this and not Media Foundation
//!
//! Because Media Foundation does not do it. See [`super::codec_mf_decode`] for
//! the measurements: the only registered H.264 decoder transform on Windows
//! accepts a D3D11 device manager, hands back D3D11-backed output samples, and
//! decodes on the CPU anyway, at 11 ms a frame for 1080p with the GPU's video
//! codec engine at 1.6%.
//!
//! ## Why libavcodec and not a hand-written DXVA decoder
//!
//! A DXVA decoder does not parse H.264 — the caller has to read the sequence
//! and picture parameter sets and every slice header, and hand the driver a
//! filled-in `DXVA_PicParams_H264`. libavcodec already contains that, hardened
//! across every GPU vendor for a decade, and it is what Moonlight uses for
//! exactly this job. Reimplementing it means owning reference picture list
//! construction, where a wrong entry produces a corrupt picture rather than an
//! error.
//!
//! [`super::h264_bitstream`] is the start of that hand-written path, kept as
//! the fallback for builds without ffmpeg.
//!
//! ## The device is ours, deliberately
//!
//! The decoded frame is an `ID3D11Texture2D` that [`super::decode_rgba`] is
//! about to colour convert. Letting libavcodec create its own device would put
//! that texture on a different device and force a cross-device share before
//! anything could read it. Handing it ours means the decode output lands where
//! it is already needed.

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::{Context, Result};
use tracing::*;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};

use super::codec_h264::DecodedPicture;
use super::decode_rgba::RgbaConverter;

/// Opaque handle to the C shim's decoder. See `tain-core/ffmpeg/db_decode.c`.
#[repr(C)]
struct DbDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn db_decoder_new(
        device: *mut core::ffi::c_void,
        codec: i32,
        err: *mut u8,
        err_len: i32,
    ) -> *mut DbDecoder;
    fn db_decoder_name(d: *mut DbDecoder) -> *const i8;
    fn db_decoder_send(d: *mut DbDecoder, data: *const u8, len: i32) -> i32;
    fn db_decoder_receive(
        d: *mut DbDecoder,
        texture: *mut *mut core::ffi::c_void,
        slice: *mut i32,
        width: *mut i32,
        height: *mut i32,
    ) -> i32;
    fn db_decoder_free(d: *mut DbDecoder);
}

/// A decoded frame that never left the GPU.
///
/// # This owns the surface it names
///
/// The texture belongs to the decoder's colour converter, and the render thread
/// that draws it runs on its own schedule — it holds the last frame it was
/// given until a newer one arrives, which on a stalled stream is never. Two
/// separate things can therefore free the surface out from under it: a rebuild
/// of the converter on a resolution change, and the end of the session, which
/// drops the whole decoder.
///
/// Handshakes fixed the first — `FfmpegDecoder::retired` and
/// `release_pictures_except` — and could not fix the second, because at
/// teardown there is nobody left to negotiate with. Both are answered instead
/// by holding a reference: while this value is alive its surface is, whatever
/// happens to the converter or the decoder behind it. The retirement stays as
/// belt and braces and could now be removed.
///
/// The address is still carried separately as a `usize`, because that is what
/// `WGL_NV_DX_interop2` takes and what the render thread caches registrations
/// against. [`Self::generation`] remains the guard for a *reused* address.
#[derive(Debug, Clone)]
pub struct GpuPicture {
    /// Keeps [`Self::texture`] pointing at a live surface. Never dereferenced
    /// here; its whole job is the reference count, which is why it is never
    /// read and must not be removed for being unread.
    #[allow(dead_code, reason = "held for its reference count, not its value")]
    keep: KeptTexture,
    pub texture: usize,
    pub width: u16,
    pub height: u16,
    /// Bumped every time the colour converter is rebuilt.
    ///
    /// # Why a pointer is not enough to identify a texture
    ///
    /// The converter is rebuilt whenever the stream changes size, which frees
    /// its textures and allocates new ones — and an allocator reuses addresses.
    /// A consumer that caches anything against the raw pointer will therefore
    /// hand back state belonging to a texture that no longer exists, which is a
    /// use-after-free and crashes.
    ///
    /// This was not a theory. A real session changed resolution five times in
    /// two minutes, and the guest died.
    pub generation: u64,
}

impl GpuPicture {
    /// Wrap a surface the caller already holds.
    ///
    /// The decoder builds its own; this is for anyone holding a texture from
    /// elsewhere — in practice a test that needs a frame without a decode
    /// pipeline behind it. Taking the interface rather than an address is the
    /// point: there is no way to make one of these that does not own what it
    /// names.
    pub fn from_texture(texture: ID3D11Texture2D, width: u16, height: u16, generation: u64) -> Self {
        use windows::core::Interface;
        let address = texture.as_raw() as usize;
        Self { keep: KeptTexture(texture), texture: address, width, height, generation }
    }
}

/// A reference to a decoded texture, carried across to the render thread.
///
/// # Why this needs an unsafe `Send`
///
/// `ID3D11Texture2D` is a COM interface and `windows` does not mark those
/// `Send`, because in general a COM object may be apartment-threaded and must
/// be used only on the thread that made it. D3D11 is not: its objects are free
/// threaded, the device here is created with multithread protection on, and the
/// only thing done with this handle off the decode thread is holding it and
/// eventually releasing it. `AddRef` and `Release` are interlocked operations.
///
/// The same reasoning already carries `Nv12Converter` and `HdrToSdr` to the
/// capture thread; this is the narrower case, being one texture and no calls.
#[derive(Debug, Clone)]
pub struct KeptTexture(
    #[allow(dead_code, reason = "held for its reference count, not its value")] ID3D11Texture2D,
);

// SAFETY: as documented above — a D3D11 resource handle, moved between threads
// and released on one of them, never called on either.
unsafe impl Send for KeptTexture {}
unsafe impl Sync for KeptTexture {}

/// H.264 decoding on the GPU, for real this time.
pub struct FfmpegDecoder {
    handle: *mut DbDecoder,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// GPU colour conversion, cached against the size it was built for.
    converter: Option<(RgbaConverter, u16, u16)>,
    /// How many times the converter has been built. See [`GpuPicture`].
    converter_generation: u64,
    /// Converters replaced by a rebuild, with the generation each one served.
    ///
    /// # Why the old textures are not freed at once
    ///
    /// Whoever draws these frames is on another thread and holds the last
    /// picture it was given — that is the whole point of handing over a texture
    /// instead of pixels. Until a newer frame reaches it, it keeps drawing the
    /// one it has.
    ///
    /// Freeing the textures the moment the new ones exist therefore pulls a
    /// live surface out from under the render thread. `generation` does not
    /// help: it distinguishes a *reused address* from an old one, and here the
    /// address is not reused, it is dangling — the consumer's cache hits, the
    /// driver is handed a freed surface, and the process dies without a panic.
    /// Reported twice, and visible in the log as a converter rebuilt two
    /// seconds before the last line.
    ///
    /// # What keeps this bounded
    ///
    /// Two things can be holding a picture, so two are kept, and everything
    /// else goes at the next rebuild.
    ///
    /// The render thread says which generation it is drawing, through
    /// [`Self::release_pictures_except`] — that covers a consumer stalled for
    /// any length of time, which is the case a fixed number of generations
    /// cannot cover.
    ///
    /// The newest retired one is kept unconditionally, because a decoded frame
    /// can be sitting in the channel between here and the render thread, at a
    /// generation the render thread has not claimed yet and never will until it
    /// picks the frame up. That queued frame is always from the generation
    /// immediately before the current one.
    retired: Retired<RgbaConverter>,
    /// Whether the caller will draw the converted texture itself.
    ///
    /// Set by the guest once its render thread has established that this driver
    /// can share Direct3D textures with OpenGL. Until then — and forever, on a
    /// driver that cannot — the frame is read back to the CPU as it always was.
    /// A per-frame decision rather than a per-session one, because the render
    /// thread learns the answer after the first frames have already arrived.
    present_on_gpu: bool,
    rgba: Vec<u8>,
    pub name: String,

    stage_send: std::time::Duration,
    stage_receive: std::time::Duration,
    stage_blit: std::time::Duration,
    stage_read: std::time::Duration,
    frames: u32,
    reported_at: std::time::Instant,
}

// SAFETY: as for the other decoders — driven from exactly one thread (the
// guest's receive task) and never shared. The raw handle is owned solely here.
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    /// A decoder for H.264, which is what most sessions run.
    pub fn new() -> Result<Self> {
        Self::for_codec(super::encoder::VideoCodec::H264)
    }

    /// A decoder for whichever codec the host said it would send.
    ///
    /// D3D11VA decodes HEVC through the same hwaccel and the same surface
    /// handling as H.264 — the only difference that reaches this side is the
    /// codec id handed to libavcodec, which is why nothing below changes.
    pub fn for_codec(codec: super::encoder::VideoCodec) -> Result<Self> {
        use super::encoder::VideoCodec;
        let codec_id = match codec {
            VideoCodec::H264 => 0,
            VideoCodec::H265 => 1,
            VideoCodec::Av1 => 2,
            other => anyhow::bail!("{} is not a codec this decoder handles", other.label()),
        };
        Self::open(codec_id)
    }

    fn open(codec_id: i32) -> Result<Self> {
        super::ffmpeg_runtime::require()?;
        let (device, context) = create_device()?;

        let mut err = [0u8; 256];
        // SAFETY: the device outlives the handle (both are owned by the struct
        // below, and the shim raises its own reference). The error buffer is
        // valid for its stated length.
        let handle = unsafe {
            db_decoder_new(device.as_raw(), codec_id, err.as_mut_ptr(), err.len() as i32)
        };
        if handle.is_null() {
            let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
            let message = String::from_utf8_lossy(&err[..end]).into_owned();
            anyhow::bail!("libavcodec would not start a D3D11VA decoder: {message}");
        }

        // SAFETY: the handle is non-null and the name is a static string owned
        // by libavcodec, valid for the lifetime of the decoder.
        let name = unsafe { std::ffi::CStr::from_ptr(db_decoder_name(handle)) }
            .to_string_lossy()
            .into_owned();

        info!("H.264 decoding on the GPU via libavcodec {name} (D3D11VA)");
        Ok(Self {
            handle,
            device,
            context,
            converter: None,
            converter_generation: 0,
            retired: Retired::default(),
            present_on_gpu: false,
            rgba: Vec::new(),
            name,
            stage_send: std::time::Duration::ZERO,
            stage_receive: std::time::Duration::ZERO,
            stage_blit: std::time::Duration::ZERO,
            stage_read: std::time::Duration::ZERO,
            frames: 0,
            reported_at: std::time::Instant::now(),
        })
    }

    /// Decode one Annex-B packet.
    ///
    /// Returns `None` while the decoder is still building state — the first
    /// packets of a stream produce no picture, which every caller handles.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        if packet.is_empty() {
            return Ok(None);
        }

        let t = std::time::Instant::now();
        // SAFETY: the packet outlives the call; the shim copies what it keeps.
        let rc = unsafe { db_decoder_send(self.handle, packet.as_ptr(), packet.len() as i32) };
        self.stage_send += t.elapsed();
        anyhow::ensure!(rc >= 0, "libavcodec rejected a packet: error {rc}");

        let mut picture = None;
        loop {
            let mut texture = std::ptr::null_mut();
            let (mut slice, mut width, mut height) = (0i32, 0i32, 0i32);

            let t = std::time::Instant::now();
            // SAFETY: all four out-params are valid; the shim only writes them
            // when it returns 1.
            let rc = unsafe {
                db_decoder_receive(
                    self.handle,
                    &mut texture,
                    &mut slice,
                    &mut width,
                    &mut height,
                )
            };
            self.stage_receive += t.elapsed();

            anyhow::ensure!(rc >= 0, "libavcodec decode failed: error {rc}");
            if rc == 0 {
                break;
            }

            // Newest wins: an older picture is stale by the time a later one
            // exists, and drawing it would only add latency.
            if let Some(decoded) = self.convert(texture, slice as u32, width, height)? {
                picture = Some(decoded);
            }
        }

        self.frames += 1;
        self.report();
        Ok(picture)
    }

    /// Colour convert one decoded surface and read it back.
    fn convert(
        &mut self,
        texture: *mut core::ffi::c_void,
        slice: u32,
        width: i32,
        height: i32,
    ) -> Result<Option<DecodedPicture>> {
        anyhow::ensure!(!texture.is_null(), "decoder returned a null texture");
        anyhow::ensure!(width > 0 && height > 0, "decoder returned {width}x{height}");
        let (w, h) = (width as u16, height as u16);

        // SAFETY: the shim hands back a borrowed ID3D11Texture2D that stays
        // valid until `db_decoder_release` below. `from_raw_borrowed` does not
        // take ownership, so the reference count is left alone.
        let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&texture) }
            .context("decoded surface is not an ID3D11Texture2D")?;

        self.ensure_converter(w, h)?;
        let Some((converter, _, _)) = self.converter.as_mut() else {
            anyhow::bail!("no colour converter for {w}x{h}");
        };

        // # Why this does NOT pipeline, having tried it
        //
        // Submitting the blit and mapping it in the same breath waits for your
        // own GPU work — 1.2 to 3 ms of the decode. Reading the *previous*
        // frame's blit instead makes that a plain memcpy, and measured well:
        // 3.71 ms to 2.02 ms a frame, 241 fps to 413.
        //
        // It was still wrong, and was reverted. Returning the previous picture
        // means a frame is only displayed once the *next* one arrives, so it
        // adds one inter-frame interval of latency — about 17 ms at 60 fps.
        // Trading 1.7 ms of decode for 17 ms of latency is a bad deal for a
        // remote desktop, where what the user feels is the delay between moving
        // the mouse and seeing it move.
        //
        // Throughput was never the constraint: 241 fps is already far above any
        // rate this streams at. The right way to remove this cost is to stop
        // reading the frame back at all — see the zero-copy present work — not
        // to hide the wait behind a frame of lag.
        let t = std::time::Instant::now();
        let submitted = converter.submit(texture, slice);
        self.stage_blit += t.elapsed();
        // Released only once the blit is submitted; the shim holds two frames so
        // the surface outlives the submission.
        submitted?;

        // The readback, and the whole reason this path exists to be skipped.
        // Measured at 3.07 ms on the machine this was written on and 8.83 ms on
        // a real guest — ninety per cent of that guest's decode, on a smaller
        // frame. When the caller is going to draw the texture itself, none of it
        // happens: no staging copy, no map, no eight megabytes allocated and
        // zeroed to be immediately overwritten.
        if self.present_on_gpu {
            return Ok(Some(DecodedPicture {
                width: w as usize,
                height: h as usize,
                rgba: Vec::new(),
            }));
        }

        let t = std::time::Instant::now();
        let read = converter.read(&mut self.rgba);
        self.stage_read += t.elapsed();
        read?;

        Ok(Some(DecodedPicture {
            width: w as usize,
            height: h as usize,
            rgba: std::mem::take(&mut self.rgba),
        }))
    }

    /// The D3D11 device the decoded textures belong to.
    ///
    /// Needed by `wglDXOpenDeviceNV`, which shares a whole device rather than a
    /// texture at a time.
    pub fn device_raw(&self) -> *mut core::ffi::c_void {
        use windows::core::Interface;
        self.device.as_raw()
    }

    /// Ask for frames to be left on the GPU, or read back as before.
    pub fn set_present_on_gpu(&mut self, on: bool) {
        if self.present_on_gpu != on {
            info!(
                "Decoded frames are now {}",
                if on { "drawn straight from the GPU" } else { "read back to the CPU" }
            );
            self.present_on_gpu = on;
        }
    }

    /// The frame `decode` last produced, if it was left on the GPU.
    ///
    /// `None` whenever the readback path ran, which is what the caller falls
    /// back to.
    pub fn last_gpu_picture(&self) -> Option<GpuPicture> {
        if !self.present_on_gpu {
            return None;
        }
        let (converter, w, h) = self.converter.as_ref()?;
        // Taken once, so the address and the reference cannot disagree about
        // which surface this is.
        let keep = converter.last_texture();
        let texture = {
            use windows::core::Interface;
            keep.as_raw() as usize
        };
        Some(GpuPicture {
            keep: KeptTexture(keep),
            texture,
            width: *w,
            height: *h,
            generation: self.converter_generation,
        })
    }

    /// Free every retired converter except the ones still reachable.
    ///
    /// `held` is the generation the render thread is currently drawing, or 0 if
    /// it is drawing none of them. See [`Self::retired`] for why the newest is
    /// kept regardless of what is held.
    ///
    /// Called from the decode loop rather than on a timer, so the memory is
    /// returned as soon as the consumer has moved on and never later than the
    /// next frame after that.
    pub fn release_pictures_except(&mut self, held: u64) {
        self.retired.release_except(held);
    }

    fn ensure_converter(&mut self, w: u16, h: u16) -> Result<()> {
        if matches!(&self.converter, Some((_, cw, ch)) if (*cw, *ch) == (w, h)) {
            return Ok(());
        }
        let converter = RgbaConverter::new(&self.device, &self.context, w, h)
            .with_context(|| format!("no GPU colour conversion for {w}x{h}"))?;
        // Retired rather than dropped — see `retired`. The generation recorded
        // is the one the outgoing converter's pictures carry, which is the
        // count before this rebuild is counted.
        let outgoing = self.converter_generation;
        if let Some((previous, _, _)) = self.converter.take() {
            self.retired.retire(outgoing, previous);
        }
        self.converter = Some((converter, w, h));
        self.converter_generation += 1;
        Ok(())
    }

    /// Report where the last five seconds of decode time went.
    fn report(&mut self) {
        if self.reported_at.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        let n = self.frames.max(1) as f64;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n;
        info!(
            "decode stages over {} frames (libavcodec D3D11VA): send {:.2}, receive {:.2}, \
             blit {:.2}, read {:.2} ms",
            self.frames,
            ms(self.stage_send),
            ms(self.stage_receive),
            ms(self.stage_blit),
            ms(self.stage_read),
        );
        self.stage_send = std::time::Duration::ZERO;
        self.stage_receive = std::time::Duration::ZERO;
        self.stage_blit = std::time::Duration::ZERO;
        self.stage_read = std::time::Duration::ZERO;
        self.frames = 0;
        self.reported_at = std::time::Instant::now();
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // SAFETY: the handle was produced by `db_decoder_new` and is freed once.
        unsafe { db_decoder_free(self.handle) };
    }
}

/// A D3D11 device the decoder and the colour converter can share.
///
/// Multithread protection is not optional: libavcodec touches this device from
/// its own threads while the guest's receive task drives the converter, and a
/// driver told it has exclusive access corrupts frames rather than reporting
/// anything.
fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: both out-params are valid; failure leaves them None.
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
    let context = context.context("D3D11CreateDevice returned no context")?;
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: the device is live; this only sets a flag on it.
        unsafe { let _ = mt.SetMultithreadProtected(true); };
    }
    Ok((device, context))
}

/// Converters kept alive past their rebuild, and the rule for letting them go.
///
/// Generic over what is held so the rule can be tested without a GPU: getting
/// it wrong costs a use-after-free on the render thread, and the cases that
/// distinguish a right rule from a plausible one are all about the order things
/// happen in, not about D3D11.
struct Retired<T> {
    kept: Vec<(u64, T)>,
}

impl<T> Default for Retired<T> {
    fn default() -> Self {
        Self { kept: Vec::new() }
    }
}

impl<T> Retired<T> {
    /// Set aside the converter that served `generation`.
    fn retire(&mut self, generation: u64, converter: T) {
        self.kept.push((generation, converter));
    }

    /// Let go of everything but what is still reachable.
    ///
    /// `held` is the generation the render thread says it is drawing, or 0 for
    /// none. The newest is kept whatever the answer, because a decoded frame
    /// can be in the channel at a generation nobody has claimed yet — see
    /// [`FfmpegDecoder::retired`].
    fn release_except(&mut self, held: u64) {
        let newest = self.kept.last().map_or(0, |(generation, _)| *generation);
        self.kept.retain(|(generation, _)| *generation == held || *generation == newest);
    }

    #[cfg(test)]
    fn generations(&self) -> Vec<u64> {
        self.kept.iter().map(|(generation, _)| *generation).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture must outlive the converter that produced it.
    ///
    /// This is the disconnect crash, in the smallest form that reproduces it. A
    /// guest draws the last frame it was handed until a newer one arrives; when
    /// the session ends there is no newer one, the decoder and its converters go,
    /// and the render thread draws a freed surface. It exits without a panic,
    /// which is why the log simply stops.
    ///
    /// The generation handshake cannot cover this — at teardown there is nobody
    /// left to hand a generation to — so the picture holds a reference instead.
    /// Here the converter is dropped while the picture is still held, and the
    /// surface is then asked to describe itself: a freed one cannot.
    #[test]
    fn a_picture_keeps_its_surface_alive_after_the_converter_is_gone() {
        use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;

        let Ok((device, context)) = create_device() else {
            eprintln!("no D3D11 device — skipping");
            return;
        };
        let (w, h) = (640u16, 360u16);
        let Ok(converter) = RgbaConverter::new(&device, &context, w, h) else {
            eprintln!("no GPU colour conversion here — skipping");
            return;
        };

        let picture = GpuPicture::from_texture(converter.last_texture(), w, h, 1);
        let address = picture.texture;

        // Everything the surface came from goes, exactly as it does when a
        // session ends: the converter first, then the device it was made on.
        drop(converter);
        drop(context);
        drop(device);

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the picture holds a reference, so this is a live texture —
        // which is the claim under test. Writing into an owned descriptor.
        unsafe { picture.keep.0.GetDesc(&mut desc) };

        assert_eq!(
            (desc.Width, desc.Height),
            (w as u32, h as u32),
            "the surface no longer describes itself, so nothing was keeping it alive"
        );
        assert_eq!(
            picture.texture, address,
            "the address a picture reports must not move while it is held"
        );
    }

    /// The two things that can still be holding a retired converter.
    ///
    /// A rebuilt converter's textures are freed on the decode thread while the
    /// render thread may still be drawing from them. Two claims have to be
    /// honoured and they are not the same claim, which is the whole subtlety:
    /// the generation the render thread says it is drawing, and the newest
    /// retired one, which covers a decoded frame sitting in the channel at a
    /// generation the render thread has not picked up and so cannot name.
    ///
    /// Dropping either rule leaves something that looks right and crashes
    /// rarely, so both are pinned here.
    #[test]
    fn a_retired_converter_is_kept_while_anything_can_still_reach_it() {
        let mut retired = Retired::default();

        // Generation 1 is retired while the render thread is drawing it.
        retired.retire(1, "first");
        retired.release_except(1);
        assert_eq!(retired.generations(), vec![1], "the frame on screen came from this one");

        // It stays through further rebuilds for as long as that is the answer —
        // a stalled consumer is exactly the case a fixed number of generations
        // cannot cover.
        retired.retire(2, "second");
        retired.release_except(1);
        retired.retire(3, "third");
        retired.release_except(1);
        assert_eq!(
            retired.generations(),
            vec![1, 3],
            "the drawn one, and the newest for whatever is in the channel — but not 2, \
             which nothing can reach any more"
        );

        // The render thread picks up the queued frame and says so.
        retired.release_except(3);
        assert_eq!(retired.generations(), vec![3], "1 is finally free");

        // And with nothing being drawn, only the channel's claim remains.
        retired.retire(4, "fourth");
        retired.release_except(0);
        assert_eq!(retired.generations(), vec![4]);
    }

    /// Holding nothing must not free the frame that is on its way.
    ///
    /// The render thread reports 0 both before it has ever drawn a GPU frame
    /// and after it lets one go. Treating that as "free everything" would drop
    /// the converter behind a frame already decoded and queued — which is the
    /// crash, arrived at from the other direction.
    #[test]
    fn nothing_being_drawn_does_not_free_what_is_still_in_flight() {
        let mut retired = Retired::default();
        retired.retire(7, "in flight");
        retired.release_except(0);
        assert_eq!(retired.generations(), vec![7]);
    }

    /// Where hardware decode time goes, against the Media Foundation numbers.
    ///
    /// Run with `cargo test --release -p tain-core -- --ignored --nocapture
    /// ffmpeg_stage_cost`. The counterpart in `codec_mf_decode` is the thing to
    /// compare against, and it uses the same content at the same size.
    #[test]
    #[ignore = "diagnostic — run explicitly, in release"]
    fn ffmpeg_stage_cost() {
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::{EncodeConfig, PixelFormat};

        let (w, h) = (1920u16, 1080u16);
        let config = EncodeConfig { fps: 60, ..EncodeConfig::default() };
        let Ok(mut encoder) = MediaFoundationEncoder::new(&config, w, h) else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let mut decoder = FfmpegDecoder::new().expect("D3D11VA decoder");

        let (wu, hu) = (w as usize, h as usize);
        let mut src = vec![0u8; wu * hu * 4];

        // Encode everything first: interleaving puts the encoder's work in the
        // same GPU queue and measures the sum rather than the decode.
        const FRAMES: usize = 120;
        let mut packets = Vec::with_capacity(FRAMES);
        for frame in 0..FRAMES {
            for y in 0..hu {
                for x in 0..wu {
                    let px = &mut src[(y * wu + x) * 4..][..4];
                    px[0] = (x + y + frame * 7) as u8;
                    px[1] = (y * 2 + frame * 3) as u8;
                    px[2] = (x * 3 + frame * 11) as u8;
                    px[3] = 255;
                }
            }
            let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else { continue };
            if !packet.is_empty() {
                packets.push(packet);
            }
        }
        drop(encoder);

        let bytes: usize = packets.iter().map(|p| p.len()).sum();
        println!(
            "{} packets, {:.0} KB each, {:.1} Mbit/s at 60 fps",
            packets.len(),
            bytes as f64 / packets.len() as f64 / 1024.0,
            bytes as f64 / packets.len() as f64 * 8.0 * 60.0 / 1e6,
        );

        let mut decoded = 0u32;
        let wall = std::time::Instant::now();
        for packet in &packets {
            if decoder.decode(packet).expect("decode").is_some() {
                decoded += 1;
            }
        }
        let wall = wall.elapsed();

        let n = decoded.max(1) as f64;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n;
        println!(
            "
{decoded} frames at {w}x{h} (libavcodec D3D11VA):
               send    {:.2} ms
  receive {:.2} ms
  blit    {:.2} ms
  read    {:.2} ms
               TOTAL   {:.2} ms
  wall    {:.2} ms/frame = {:.0} fps
",
            ms(decoder.stage_send),
            ms(decoder.stage_receive),
            ms(decoder.stage_blit),
            ms(decoder.stage_read),
            ms(decoder.stage_send
                + decoder.stage_receive
                + decoder.stage_blit
                + decoder.stage_read),
            ms(wall),
            n / wall.as_secs_f64(),
        );
        assert!(decoded > 0, "nothing decoded across {FRAMES} frames");
    }

    /// The hardware decode must agree with the software one, pixel for pixel.
    ///
    /// # Why this and not a dimensions check
    ///
    /// A decoder wired up wrongly does not fail. It returns a picture of the
    /// right size full of the wrong pixels — a stale reference frame, a
    /// mis-parsed slice, a colour matrix off by a generation. Every other test
    /// in this file passes on garbage. This one does not.
    ///
    /// OpenH264 is the reference because it shares no code with the path under
    /// test: different parser, different reconstruction, different colour
    /// conversion. Agreement between them is evidence; agreement between two
    /// halves of the same implementation would not be.
    #[test]
    #[ignore = "requires a hardware H.264 encoder and decoder"]
    fn the_gpu_decodes_the_same_picture_the_cpu_does() {
        use crate::codec_h264::H264Decoder;
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::{EncodeConfig, PixelFormat};

        let (w, h) = (640u16, 480u16);
        let Ok(mut encoder) = MediaFoundationEncoder::new(&EncodeConfig::default(), w, h) else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let mut gpu = FfmpegDecoder::new().expect("D3D11VA decoder");
        let mut cpu = H264Decoder::new().expect("OpenH264");

        // Moving content: a static picture agrees trivially, because a decoder
        // that ignored every P-frame would still show the right keyframe.
        let (wu, hu) = (w as usize, h as usize);
        let mut src = vec![0u8; wu * hu * 4];
        let mut packets = Vec::new();
        for frame in 0..24 {
            for y in 0..hu {
                for x in 0..wu {
                    let px = &mut src[(y * wu + x) * 4..][..4];
                    // Smooth and moving, and deliberately free of wraparound.
                    // An `as u8` cast on a ramp steps 255 to 0 every so many
                    // pixels, and those cliffs are exactly where two decoders
                    // legitimately disagree — chroma is subsampled, so each
                    // reconstructs the edge with its own interpolation. That is
                    // a property of the test picture, not of the decode.
                    let (fx, fy) = (x as f64 / wu as f64, y as f64 / hu as f64);
                    let t = frame as f64 * 0.1;
                    let wave = |v: f64| (127.0 + 120.0 * v.sin()) as u8;
                    px[0] = wave(fx * 6.0 + t);
                    px[1] = wave(fy * 5.0 + t * 1.3);
                    px[2] = wave((fx + fy) * 4.0 + t * 0.7);
                    px[3] = 255;
                }
            }
            let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else { continue };
            if !packet.is_empty() {
                packets.push(packet);
            }
        }
        assert!(packets.len() > 8, "encoder produced only {} packets", packets.len());

        let mut compared = 0;
        let mut worst = 0.0f64;
        for packet in &packets {
            let from_gpu = gpu.decode(packet).expect("gpu decode");
            let from_cpu = cpu.decode(packet).expect("cpu decode");
            let (Some(a), Some(b)) = (from_gpu, from_cpu) else { continue };
            assert_eq!((a.width, a.height), (b.width, b.height), "size disagreement");

            // Mean absolute difference over RGB. Exact equality is the wrong
            // bar: the two do their YUV to RGB in different arithmetic, so a
            // point or two per channel is rounding, not a decode fault.
            let mut total = 0u64;
            let mut count = 0u64;
            let mut luma = 0f64;
            let mut bias = [0i64; 3];
            let mut worst_px = 0u8;
            for (pa, pb) in a.rgba.chunks_exact(4).zip(b.rgba.chunks_exact(4)) {
                for c in 0..3 {
                    total += pa[c].abs_diff(pb[c]) as u64;
                    bias[c] += pa[c] as i64 - pb[c] as i64;
                    worst_px = worst_px.max(pa[c].abs_diff(pb[c]));
                    count += 1;
                }
                let y = |p: &[u8]| 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
                luma += (y(pa) - y(pb)).abs();
            }
            let mad = total as f64 / count.max(1) as f64;
            let pixels = (count / 3).max(1) as f64;
            if compared == 0 {
                // Printed because the shape of a disagreement says what it is:
                // a large constant bias is a colour-space or range mismatch, a
                // large `worst` with no bias is localised corruption, and small
                // everything is two implementations rounding differently.
                println!(
                    "frame 0: mad {mad:.2}, luma mad {luma_mad:.2}, worst pixel {worst_px}, bias R{r:+.2} G{g:+.2} B{b:+.2}",
                    luma_mad = luma / pixels,
                    r = bias[0] as f64 / pixels,
                    g = bias[1] as f64 / pixels,
                    b = bias[2] as f64 / pixels,
                );
            }
            worst = worst.max(mad);
            assert!(
                mad < 3.0,
                "frame {compared}: mean absolute difference {mad:.2} between GPU and CPU decode — that is a wrong picture, not rounding"
            );
            compared += 1;
        }

        assert!(compared > 4, "only {compared} frames compared");
        println!("{compared} frames agreed, worst mean absolute difference {worst:.2}/255");
    }

    /// The decoder against a stream this project actually produces.
    #[test]
    #[ignore = "requires a hardware H.264 encoder and decoder"]
    fn a_hardware_encoded_frame_decodes_on_the_gpu() {
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::{EncodeConfig, PixelFormat};

        let (w, h) = (640u16, 480u16);
        let Ok(mut encoder) = MediaFoundationEncoder::new(&EncodeConfig::default(), w, h) else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let mut decoder = FfmpegDecoder::new().expect("D3D11VA decoder");

        let mut src = vec![0u8; w as usize * h as usize * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px[0] = (i % 200) as u8;
            px[1] = 90;
            px[2] = 210;
            px[3] = 255;
        }

        let mut decoded = 0;
        for _ in 0..30 {
            let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else { continue };
            if packet.is_empty() {
                continue;
            }
            if let Some(picture) = decoder.decode(&packet).expect("decode") {
                assert_eq!(picture.width, w as usize);
                assert_eq!(picture.height, h as usize);
                assert_eq!(picture.rgba.len(), w as usize * h as usize * 4);
                decoded += 1;
            }
        }
        assert!(decoded > 0, "nothing decoded across 30 encoded frames");
    }
}
