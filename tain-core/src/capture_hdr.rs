//! Bringing an HDR desktop back into the range the rest of the pipeline works in.
//!
//! # What changes when the host turns HDR on
//!
//! Nothing about the code, which is the problem. Windows starts compositing the
//! desktop into a `R16G16B16A16_FLOAT` surface in **scRGB**: linear light,
//! Rec.709 primaries, `1.0` at the reference white the SDR content slider is
//! set to, and values freely above that — a specular highlight is not `1.0`
//! clipped, it is `4.0`. Everything downstream of capture assumes eight-bit
//! sRGB with gamma already applied, starting with the video processor in
//! [`crate::capture_nv12`], which is told in as many words that its input is
//! `RGB_FULL_G22_NONE_P709`.
//!
//! That assumption is wrong at the very first step, and no stage after it can
//! tell. There is no marker in an eight-bit buffer that says the numbers in it
//! were derived from the wrong transfer function.
//!
//! # Why the conversion goes here rather than into the NV12 converter
//!
//! Because it is not only the NV12 path. The same acquired texture feeds the
//! raw and delta codecs and the connection preview, each of which reads it back
//! as BGRA. Putting the conversion at the point of acquisition — floating-point
//! desktop in, ordinary BGRA out — leaves every one of those paths untouched
//! and gives exactly one place that has to be right.
//!
//! # What this stage does and does not do
//!
//! It converts. It does not tone map.
//!
//! The video processor is told the truth about both ends — linear scRGB in,
//! gamma-encoded sRGB out — and applies the transfer function correctly, so
//! everything within the SDR range arrives at the right brightness and the right
//! colour. What it does not do is compress the range above reference white:
//! anything brighter than `1.0` clips. On a desktop that is mostly windows and
//! text that is nearly all of the picture; on a bright sky or a specular
//! highlight it is a flat white patch where there should be detail.
//!
//! That is a deliberate first step rather than an oversight. It is a small,
//! predictable change that uses hardware already in the pipeline, and it is
//! worth seeing on real content before writing a tone curve — which needs a
//! shader, and the display's real luminance range, and somewhere to put both.
//! [`DisplayColour`] already carries the luminance figures for when it does.

use anyhow::{Context, Result};
use tracing::*;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorEnumerator1,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIOutput, IDXGIOutput6};

/// The colour space a desktop surface is in when Windows is compositing HDR.
///
/// Linear light — `G10` is a gamma of 1.0 — with Rec.709 primaries and no
/// range restriction. Not PQ and not Rec.2020: those describe the signal sent
/// to the panel, which is several steps further on and not what duplication
/// hands over.
const SCRGB: DXGI_COLOR_SPACE_TYPE = DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709;

/// What the rest of the pipeline believes it is receiving, and now really is.
const SDR: DXGI_COLOR_SPACE_TYPE = DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;

/// What an output says about its own colour handling.
///
/// Read once, when duplication is set up. The luminance figures are not used
/// yet; they are what a tone curve would be built from, and they are read here
/// because this is the only place that has the output to ask.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayColour {
    /// The space the *panel* is being driven in. HDR10 here means the desktop
    /// is being composited in [`SCRGB`].
    pub space: DXGI_COLOR_SPACE_TYPE,
    /// Bits per colour channel the panel accepts. 10 does not imply HDR: plenty
    /// of SDR panels are ten-bit.
    pub bits_per_colour: u32,
    /// Darkest and brightest the panel claims, in nits.
    pub min_nits: f32,
    pub max_nits: f32,
    /// Brightest it can hold across the *whole* screen, which for most panels
    /// is far below `max_nits` — that one is a small-window measurement.
    pub max_full_frame_nits: f32,
}

impl DisplayColour {
    /// Whether this output is in HDR right now.
    ///
    /// Asked of the colour space rather than the luminance or the bit depth,
    /// because those describe what the panel *could* do. A 10-bit, 500-nit
    /// laptop screen with HDR switched off still composites an ordinary sRGB
    /// desktop, and capturing it needs none of this.
    pub fn is_hdr(self) -> bool {
        self.space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
    }

    /// The format duplication should be asked for.
    ///
    /// Requesting floating point for an SDR desktop is not free: DXGI would
    /// convert an eight-bit surface up to sixteen-bit float and every frame
    /// would cross the bus at twice the size, for no more information than it
    /// started with.
    pub fn duplication_format(self) -> DXGI_FORMAT {
        if self.is_hdr() {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }
}

/// Ask an output what it is doing, if it is new enough to be asked.
///
/// `None` for a Windows or driver too old for `IDXGIOutput6`, which is also a
/// Windows too old to have HDR desktops at all — so the answer would have been
/// "SDR" either way, and treating "cannot ask" as "not HDR" costs nothing.
pub fn probe(output: &IDXGIOutput) -> Option<DisplayColour> {
    let output6 = output.cast::<IDXGIOutput6>().ok()?;
    // SAFETY: a documented read into a local, on a live interface.
    let desc = unsafe { output6.GetDesc1() }.ok()?;
    Some(DisplayColour {
        space: desc.ColorSpace,
        bits_per_colour: desc.BitsPerColor,
        min_nits: desc.MinLuminance,
        max_nits: desc.MaxLuminance,
        max_full_frame_nits: desc.MaxFullFrameLuminance,
    })
}

/// Converts a floating-point HDR desktop into the BGRA the pipeline expects.
///
/// Built lazily, when a frame actually arrives in floating point — see the
/// module documentation for why that is the trigger rather than the display's
/// own answer.
pub struct HdrToSdr {
    video_device: ID3D11VideoDevice,
    video: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    target: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    /// Cached per source texture. Duplication hands back a small rotating set
    /// of surfaces, so this hits almost every frame.
    input_view: Option<(*mut core::ffi::c_void, ID3D11VideoProcessorInputView)>,
    width: u16,
    height: u16,
}

impl HdrToSdr {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "display is {width}x{height}");

        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("this device has no video support — was VIDEO_SUPPORT requested?")?;
        let video: ID3D11VideoContext1 = context
            .cast()
            .context("device context is not a ID3D11VideoContext1 (needs D3D11.1)")?;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: width as u32,
            InputHeight: height as u32,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: width as u32,
            OutputHeight: height as u32,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        // SAFETY: desc is fully initialised and outlives the call.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("no video processor for this content description")?;

        // Asked rather than assumed, exactly as the NV12 converter does. Here it
        // matters more: floating-point input is a less well-trodden path than
        // BGRA, and a driver that refuses it should leave the caller with the
        // frame it already had rather than a blit that fails every frame.
        if let Ok(enumerator1) = enumerator.cast::<ID3D11VideoProcessorEnumerator1>() {
            // SAFETY: plain value arguments.
            let supported = unsafe {
                enumerator1.CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                    SCRGB,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    SDR,
                )
            }
            .context("could not query scRGB to BGRA support")?;
            anyhow::ensure!(
                supported.as_bool(),
                "this GPU will not convert a floating-point HDR desktop to BGRA"
            );
        }

        // SAFETY: the enumerator outlives the processor it produces.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("could not create the video processor")?;

        // SAFETY: processor is live; both colour spaces are valid enum values.
        unsafe {
            video.VideoProcessorSetStreamColorSpace1(&processor, 0, SCRGB);
            video.VideoProcessorSetOutputColorSpace1(&processor, SDR);
            video.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
        }

        let target = create_bgra_target(device, width, height)?;

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
        .context("could not create the HDR output view")?;

        Ok(Self {
            video_device,
            video,
            processor,
            enumerator,
            target,
            output_view: output_view.context("output view came back empty")?,
            input_view: None,
            width,
            height,
        })
    }

    /// Convert one floating-point desktop frame. The result is BGRA and stays
    /// on the GPU, so the caller can hand it straight to whatever it was going
    /// to do with the original.
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let input = self.input_view_for(source)?;

        // Fill the target exactly, or the driver may letterbox — a black border
        // rather than an error, as the NV12 converter found.
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

        // SAFETY: processor, output view and input surface are live for the
        // call. `stream` holds a ManuallyDrop clone, released below.
        let blit = unsafe {
            self.video.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        let mut stream = stream;
        // SAFETY: taken exactly once, and `stream` is not used again. Leaking
        // one reference per frame would pin a texture forever.
        unsafe { core::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        blit.context("VideoProcessorBlt failed converting the HDR desktop")?;

        // Cloned rather than borrowed: the caller goes on to use its own
        // converter mutably, and a reference into this one would forbid that.
        // A COM clone is an increment, not a copy of the pixels.
        Ok(self.target.clone())
    }

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
        .context("could not create the HDR input view")?;

        let view = view.context("input view came back empty")?;
        self.input_view = Some((key, view.clone()));
        Ok(view)
    }
}

// The cached raw pointer is a comparison key, never dereferenced, and the
// interface beside it owns the reference that keeps the object alive. Same
// reasoning as `Nv12Converter`, which sits beside this in the same capturer and
// is moved to the capture thread with it.
unsafe impl Send for HdrToSdr {}
unsafe impl Sync for HdrToSdr {}

fn create_bgra_target(
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
        Usage: D3D11_USAGE_DEFAULT,
        // Render target for the blit; shader resource because the NV12
        // converter takes this texture as *its* input on the next step.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: desc is fully initialised; the out-param is checked.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("could not create the HDR conversion target")?;
    texture.context("conversion target came back empty")
}

/// What has already been said about the display, so it is not said again.
///
/// # Why this is not simply a log line
///
/// The duplication it belongs to is rebuilt whenever it is lost, and when the
/// rebuild keeps failing that is about twenty-five times a second. A line
/// written unconditionally during setup is a line written twenty-five times a
/// second: the log that first showed the HDR fault is two messages repeated for
/// its whole length, and the one that mattered — the duplication call that
/// failed — is buried in them.
///
/// So this outlives the rebuild, and reports only what changed since last time.
#[derive(Default)]
pub struct ColourLog {
    /// The last answer reported. The outer `Option` separates "nothing has been
    /// reported yet" from "an output that would not answer was reported".
    reported: Option<Option<DisplayColour>>,
    /// Whether this GPU has already been reported as unable to convert.
    reported_refusal: bool,
}

impl ColourLog {
    /// Say what the display is doing, if that is not what was said last time.
    ///
    /// Worth saying at all — even where there is no HDR anywhere — because "the
    /// picture is washed out" and "the picture is fine" look identical in a log
    /// that never mentions which of the two colour pipelines was in use.
    /// Returns whether anything was written, which is what the tests check:
    /// the throttle is the entire point of this type and it is invisible from
    /// the outside otherwise.
    pub fn display(&mut self, colour: Option<DisplayColour>) -> bool {
        if self.reported == Some(colour) {
            return false;
        }
        self.reported = Some(colour);

        let Some(colour) = colour else {
            debug!("Display colour: could not be read — treating the desktop as SDR");
            return true;
        };
        if colour.is_hdr() {
            info!(
                "Display is in HDR — {} bits per colour, {:.0}-{:.0} nits ({:.0} full frame)",
                colour.bits_per_colour,
                colour.min_nits,
                colour.max_nits,
                colour.max_full_frame_nits,
            );
        } else {
            debug!(
                "Display is SDR — {} bits per colour, up to {:.0} nits",
                colour.bits_per_colour, colour.max_nits,
            );
        }
        true
    }

    /// Report, once, that this GPU will not do the conversion.
    ///
    /// Once, because it will not start being able to. Capture carries on in
    /// eight bits, which DXGI converts on its own terms.
    pub fn conversion_refused(&mut self, reason: &anyhow::Error) -> bool {
        if self.reported_refusal {
            return false;
        }
        self.reported_refusal = true;
        warn!(
            "This GPU will not convert the floating-point HDR desktop ({reason:#}) — \
             capturing it as eight-bit instead, which DXGI converts on its own terms \
             and may look washed out"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdr() -> DisplayColour {
        DisplayColour {
            space: DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            bits_per_colour: 8,
            min_nits: 0.5,
            max_nits: 270.0,
            max_full_frame_nits: 270.0,
        }
    }

    /// A capable panel is not the same as a panel in HDR mode.
    ///
    /// The machine this was written on has exactly the trap: a ten-bit screen
    /// reporting 507 nits, compositing an ordinary sRGB desktop because HDR is
    /// switched off. Deciding from the bit depth or the luminance would put it
    /// down the floating-point path, converting a colour space it is not in.
    #[test]
    fn a_bright_ten_bit_panel_with_hdr_off_is_not_hdr() {
        let capable = DisplayColour {
            space: DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            bits_per_colour: 10,
            min_nits: 5.0762,
            max_nits: 507.0,
            max_full_frame_nits: 507.0,
        };
        assert!(!capable.is_hdr());
        assert_eq!(capable.duplication_format(), DXGI_FORMAT_B8G8R8A8_UNORM);
    }

    #[test]
    fn an_hdr_desktop_is_captured_in_floating_point() {
        let hdr = DisplayColour {
            space: DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            bits_per_colour: 10,
            min_nits: 0.0,
            max_nits: 1000.0,
            max_full_frame_nits: 600.0,
        };
        assert!(hdr.is_hdr());
        assert_eq!(hdr.duplication_format(), DXGI_FORMAT_R16G16B16A16_FLOAT);
    }

    /// An SDR desktop must not be dragged through floating point.
    ///
    /// It would be correct and cost twice the bus bandwidth for no more
    /// information — the surface started as eight bits either way.
    #[test]
    fn an_sdr_desktop_stays_eight_bit() {
        assert!(!sdr().is_hdr());
        assert_eq!(sdr().duplication_format(), DXGI_FORMAT_B8G8R8A8_UNORM);
    }

    /// Logging must survive an output that would not answer.
    #[test]
    fn an_unreadable_display_is_not_a_panic() {
        let mut log = ColourLog::default();
        assert!(log.display(None), "the first answer is always news");
        assert!(log.display(Some(sdr())), "and so is a different one");
    }

    fn hdr() -> DisplayColour {
        DisplayColour {
            space: DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            bits_per_colour: 10,
            min_nits: 0.0,
            max_nits: 418.0,
            max_full_frame_nits: 418.0,
        }
    }

    /// A rebuild loop must not write the same line on every attempt.
    ///
    /// This is not tidiness. The duplication is rebuilt on every capture call
    /// while it cannot be created — about twenty-five times a second — and the
    /// log from the machine where HDR first failed is two messages repeated for
    /// its whole length, with the duplication error that actually explained it
    /// buried among them.
    #[test]
    fn an_unchanged_display_is_reported_once() {
        let mut log = ColourLog::default();
        assert!(log.display(Some(hdr())), "the first answer is worth saying");
        for _ in 0..100 {
            assert!(!log.display(Some(hdr())), "and saying again is not");
        }
    }

    /// A display that changes is news again — that is the event worth catching.
    #[test]
    fn switching_hdr_on_is_reported() {
        let mut log = ColourLog::default();
        assert!(log.display(Some(sdr())));
        assert!(!log.display(Some(sdr())));
        assert!(log.display(Some(hdr())), "HDR being switched on must be visible");
        assert!(!log.display(Some(hdr())));
        assert!(log.display(Some(sdr())), "and switched off again");
    }

    /// A GPU that will not convert will not start being able to.
    #[test]
    fn a_refusal_to_convert_is_reported_once() {
        let mut log = ColourLog::default();
        let reason = anyhow::anyhow!("this GPU will not convert");
        assert!(log.conversion_refused(&reason));
        for _ in 0..100 {
            assert!(!log.conversion_refused(&reason));
        }
    }

    /// What the display attached to this machine actually reports.
    ///
    /// Ignored because it needs a display, and printing rather than asserting
    /// because there is no right answer — it depends on the panel and on
    /// whether HDR is switched on. Run it to find out which of the two capture
    /// paths a machine will take, and to see the luminance figures a tone curve
    /// would have to work from:
    ///
    /// ```text
    /// cargo test -p tain-core probes_the_real_display -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a display; prints rather than asserts"]
    #[cfg(target_os = "windows")]
    fn probes_the_real_display() {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

        // SAFETY: enumeration only; every interface is dropped at the end.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("no DXGI factory");
            let mut adapter_index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                let mut output_index = 0;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    let where_ = format!("adapter {adapter_index} output {output_index}");
                    match probe(&output) {
                        Some(colour) => {
                            let verdict = if colour.is_hdr() {
                                "HDR — captured in floating point and converted"
                            } else {
                                "SDR — captured as eight-bit, unchanged"
                            };
                            println!("{where_}: {} bits/colour", colour.bits_per_colour);
                            println!(
                                "  luminance {:.4}-{:.0} nits ({:.0} full frame)",
                                colour.min_nits, colour.max_nits, colour.max_full_frame_nits,
                            );
                            println!("  colour space {} — {verdict}", colour.space.0);
                        }
                        None => println!("{where_}: would not answer"),
                    }
                    output_index += 1;
                }
                adapter_index += 1;
            }
        }
    }
}
