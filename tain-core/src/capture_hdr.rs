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
//! # What this stage does
//!
//! It converts, and it tone maps, and there are two ways it can do both.
//!
//! The first is a shader — a full-screen pass that reads the floating-point
//! desktop, rolls the range above reference white down instead of clipping it,
//! applies the sRGB transfer function and writes eight-bit BGRA. See
//! [`rolled_off`] for the curve and why it has the shape it does.
//!
//! The second is a video processor, told the truth about both ends: linear
//! scRGB in, gamma-encoded sRGB out. It gets the transfer function right, so
//! everything within the SDR range arrives at the right brightness and colour,
//! but it has nowhere to put anything brighter than `1.0` and clips it. On a
//! desktop of windows and text that is nearly all of the picture; on a bright
//! sky or a specular highlight it is a flat white patch where detail should be.
//!
//! The shader is preferred because it is strictly better on that one point and
//! costs a fraction of a millisecond at desktop sizes. The video processor
//! stays as the fallback for a machine where the shader will not build, and it
//! is not a hypothetical fallback in the other direction: a real GPU refused
//! `CheckVideoProcessorFormatConversion` for scRGB to BGRA, which left HDR with
//! no path at all until this one existed.

use anyhow::{Context, Result};
use tracing::*;

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT,
};
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
struct VideoProcessorPath {
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

/// Where the highlight roll-off starts, as a fraction of reference white.
///
/// Three quarters of the SDR range passes through untouched, which is where
/// almost all of a desktop lives, and the quarter below white absorbs the roll.
const KNEE: f32 = 0.75;

/// Compress linear light above the knee instead of clipping it at `1.0`.
///
/// # The shape, and why this one
///
/// Below `knee` the input is returned unchanged, so the overwhelming majority
/// of a desktop — text, windows, anything an SDR application drew — is bit for
/// bit what the video processor would have produced. Above it the curve
/// approaches `1.0` without ever reaching it, so a specular highlight keeps its
/// gradient instead of becoming a flat white patch.
///
/// The shoulder is shaped so that value *and slope* both match the straight
/// line at the knee. A curve that only matched the value would put a visible
/// crease across every gradient that crossed it — a sky is exactly the content
/// that spends time near the knee, and exactly the content that shows a crease.
///
/// # Why this shoulder and not an exponential
///
/// An exponential approach to `1.0` joins the line just as smoothly and is the
/// more obvious thing to write. It was written first, and it is useless: it
/// decays so fast that everything above roughly twice reference white lands on
/// 255 anyway. Measured at eight bits, `1.0` reaches 232, `1.5` reaches 252 and
/// `2.0` is already white — which is most of a real highlight range, so it
/// would have replaced clipping with clipping half a stop later.
///
/// This one falls off as `1/x`, so the same eight bits still separate `4.0`
/// from `8.0` from `16.0` (250, 253, 254) and it does not reach white until
/// about sixty times reference white, which is past anything a desktop
/// composites. It is also cheaper than `exp`.
///
/// Applied per channel, which is the ordinary choice and the cheap one. It
/// shifts hue slightly on highlights that are saturated as well as bright,
/// because the brightest channel rolls further than the others. Mapping
/// luminance and rescaling the channels would hold hue, at the cost of pushing
/// individual channels back above `1.0` and needing a second decision about
/// what to do with them.
pub fn rolled_off(x: f32, knee: f32) -> f32 {
    // Negative values are real: scRGB uses them for colours outside Rec.709.
    // Clamping is the honest cheap answer — anything better is gamut mapping.
    let x = x.max(0.0);
    if x <= knee {
        return x;
    }
    let headroom = 1.0 - knee;
    // A knee at or above white leaves nothing to roll into, so there is no
    // curve to apply and clipping is all that is left.
    if headroom <= 0.0 {
        return x.min(1.0);
    }
    1.0 - headroom * headroom / (x - knee + headroom)
}

impl VideoProcessorPath {
    fn new(
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
    fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
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

/// The full-screen pass: scRGB in, tone-mapped sRGB out.
///
/// `SV_VertexID` builds the triangle, so there is no vertex buffer and no input
/// layout — three vertices covering the whole target with UVs to match.
const TONEMAP_HLSL: &str = r#"
Texture2D<float4> Source : register(t0);
SamplerState Point : register(s0);
cbuffer Params : register(b0) { float Knee; float3 Padding; };

struct Vertex { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };

Vertex vs_main(uint id : SV_VertexID) {
    Vertex v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.pos = float4(v.uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return v;
}

// Matches `rolled_off`. Value and slope meet the line at the knee, and the
// falloff is 1/x so that highlights well above white stay distinguishable.
float roll(float x, float knee) {
    x = max(x, 0.0);
    if (x <= knee) return x;
    float headroom = 1.0 - knee;
    if (headroom <= 0.0) return min(x, 1.0);
    return 1.0 - headroom * headroom / (x - knee + headroom);
}

// The sRGB transfer function, written out rather than left to a _SRGB render
// target: the target is plain UNORM because everything downstream reads these
// bytes as ordinary sRGB, and a typed target would apply this a second time.
float encode(float c) {
    return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

float4 ps_main(Vertex v) : SV_TARGET {
    float3 c = Source.Sample(Point, v.uv).rgb;
    c = float3(roll(c.r, Knee), roll(c.g, Knee), roll(c.b, Knee));
    return float4(encode(c.r), encode(c.g), encode(c.b), 1.0);
}
"#;

/// Tone maps the floating-point desktop with a pixel shader.
struct ShaderPath {
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    /// Written by the pass and handed to the caller.
    target: ID3D11Texture2D,
    target_view: ID3D11RenderTargetView,
    /// Only allocated if the acquired surface cannot be sampled directly.
    ///
    /// A duplicated desktop arrives without `BIND_SHADER_RESOURCE` on most
    /// drivers, and a shader cannot read what it cannot bind. Copying on the
    /// GPU costs a fraction of a millisecond at desktop sizes and is skipped
    /// entirely where the flag is already there.
    readable: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    /// Cached per source texture, as the video processor path caches its views.
    source_view: Option<(*mut core::ffi::c_void, ID3D11ShaderResourceView)>,
    width: u16,
    height: u16,
}

impl ShaderPath {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "display is {width}x{height}");

        let vertex_code = compile(TONEMAP_HLSL, s!("vs_main"), s!("vs_4_0"))?;
        let pixel_code = compile(TONEMAP_HLSL, s!("ps_main"), s!("ps_4_0"))?;

        let mut vertex = None;
        let mut pixel = None;
        let mut sampler = None;
        let mut constants = None;
        let mut target = None;
        let mut target_view = None;

        let knee = [KNEE, 0.0, 0.0, 0.0];
        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of_val(&knee) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let knee_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: knee.as_ptr().cast(),
            ..Default::default()
        };

        let sampler_desc = D3D11_SAMPLER_DESC {
            // One source pixel per target pixel, so there is nothing to
            // interpolate and asking for it would only cost bandwidth.
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };

        let target_desc = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };

        // SAFETY: every descriptor is fully initialised and outlives its call;
        // each out-parameter is checked before use.
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vertex_code), None, Some(&mut vertex))
                .context("could not create the tone-mapping vertex shader")?;
            device
                .CreatePixelShader(blob_bytes(&pixel_code), None, Some(&mut pixel))
                .context("could not create the tone-mapping pixel shader")?;
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .context("could not create the sampler")?;
            device
                .CreateBuffer(&buffer_desc, Some(&knee_data), Some(&mut constants))
                .context("could not create the constant buffer")?;
            device
                .CreateTexture2D(&target_desc, None, Some(&mut target))
                .context("could not create the tone-mapped target")?;
            let created = target.clone().context("no target texture")?;
            device
                .CreateRenderTargetView(&created, None, Some(&mut target_view))
                .context("could not create the render target view")?;
        }

        Ok(Self {
            context: context.clone(),
            device: device.clone(),
            vertex: vertex.context("no vertex shader")?,
            pixel: pixel.context("no pixel shader")?,
            sampler: sampler.context("no sampler")?,
            constants: constants.context("no constant buffer")?,
            target: target.context("no target texture")?,
            target_view: target_view.context("no render target view")?,
            readable: None,
            source_view: None,
            width,
            height,
        })
    }

    fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let view = self.source_view_for(source)?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // SAFETY: every object bound here is owned by `self` and outlives the
        // draw. The pipeline is left set afterwards, which is harmless: the only
        // other user of this context is a video processor, and a blit does not
        // read graphics pipeline state.
        unsafe {
            self.context.OMSetRenderTargets(Some(&[Some(self.target_view.clone())]), None);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.IASetInputLayout(None);
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context.PSSetShaderResources(0, Some(&[Some(view)]));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context.Draw(3, 0);
            // Unbound immediately: the same texture is a render target on the
            // next frame's copy, and a driver will not let it be both.
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(None, None);
        }

        // A COM clone is an increment, not a copy of the pixels — see the
        // video processor path for why the caller needs its own reference.
        Ok(self.target.clone())
    }

    /// A shader resource view on the acquired surface, copying it first if the
    /// surface cannot be bound as one.
    fn source_view_for(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: writing into a fully owned descriptor.
        unsafe { source.GetDesc(&mut desc) };

        if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
            let key = source.as_raw();
            if let Some((cached, view)) = &self.source_view
                && *cached == key
            {
                return Ok(view.clone());
            }
            let view = self.view_of(source)?;
            self.source_view = Some((key, view.clone()));
            return Ok(view);
        }

        if self.readable.is_none() {
            let readable_desc = D3D11_TEXTURE2D_DESC {
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                MiscFlags: 0,
                CPUAccessFlags: 0,
                Usage: D3D11_USAGE_DEFAULT,
                ..desc
            };
            let mut texture = None;
            // SAFETY: descriptor is initialised; out-parameter checked below.
            unsafe {
                self.device
                    .CreateTexture2D(&readable_desc, None, Some(&mut texture))
                    .context("could not create a readable copy of the HDR desktop")?;
            }
            let texture = texture.context("no readable texture")?;
            let view = self.view_of(&texture)?;
            self.readable = Some((texture, view));
        }

        let (texture, view) = self.readable.as_ref().context("no readable copy")?;
        // SAFETY: both textures have the same descriptor bar the bind flags.
        unsafe { self.context.CopyResource(texture, source) };
        Ok(view.clone())
    }

    fn view_of(&self, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
        let mut view = None;
        // SAFETY: the texture outlives the call; out-parameter checked.
        unsafe {
            self.device
                .CreateShaderResourceView(texture, None, Some(&mut view))
                .context("could not create a shader resource view on the HDR desktop")?;
        }
        view.context("no shader resource view")
    }
}

/// Compile one entry point out of [`TONEMAP_HLSL`].
///
/// The shader is compiled here rather than built into the binary because
/// shipping bytecode needs `fxc` at build time, which is not something a clone
/// of this repository can be assumed to have. `d3dcompiler_47.dll` ships with
/// Windows.
fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source outlives the call; both out-parameters are read only
    // after the result has been inspected.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(e) = compiled {
        // The compiler's own message says which line, which is the only useful
        // thing to have when this fails.
        let detail = errors.map(|blob| {
            // SAFETY: the blob owns the buffer and is alive for the read.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                )
            };
            String::from_utf8_lossy(bytes).trim_end_matches(['\0', '\n']).to_string()
        });
        return Err(anyhow::anyhow!(
            "{}: {}",
            e,
            detail.unwrap_or_else(|| "no message".into())
        ));
    }

    code.context("the shader compiler returned no code")
}

/// The compiled bytecode, as the shader-creation calls want it.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns the buffer and outlives the borrow.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// Brings a floating-point HDR desktop back to BGRA the rest of the pipeline
/// understands, by whichever route this machine will run.
pub struct HdrToSdr(Route);

/// Which of the two the machine turned out to support. Not the caller's
/// business — the difference is in the highlights, not in the interface.
enum Route {
    /// Preferred: converts and tone maps. See [`rolled_off`].
    Shader(ShaderPath),
    /// Fallback: converts correctly and clips above reference white.
    Processor(VideoProcessorPath),
}

impl HdrToSdr {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u16,
        height: u16,
    ) -> Result<Self> {
        match ShaderPath::new(device, context, width, height) {
            Ok(shader) => {
                info!("HDR desktop will be tone mapped in a shader at {width}x{height}");
                return Ok(Self(Route::Shader(shader)));
            }
            Err(e) => {
                // Worth a warning rather than a debug line: the fallback works,
                // but it clips every highlight, and the difference is visible.
                warn!("No shader tone mapping ({e:#}) — falling back to the video processor");
            }
        }
        let processor = VideoProcessorPath::new(device, context, width, height)?;
        info!("HDR desktop will be converted by the video processor at {width}x{height}");
        Ok(Self(Route::Processor(processor)))
    }

    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        match &mut self.0 {
            Route::Shader(shader) => shader.convert(source),
            Route::Processor(processor) => processor.convert(source),
        }
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

    /// sRGB encoding, to compare the shader's output against.
    fn encode(c: f32) -> f32 {
        if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    }

    /// The knee is where the curve stops being the identity.
    ///
    /// Everything below it has to arrive exactly as the video processor would
    /// have produced it, or turning HDR on would shift the brightness of every
    /// window on the desktop — which is the failure this whole module exists to
    /// prevent, reintroduced by the fix for it.
    #[test]
    fn below_the_knee_nothing_is_touched() {
        for x in [0.0, 0.1, 0.25, 0.5, 0.74, KNEE] {
            assert_eq!(rolled_off(x, KNEE), x, "{x} is below the knee");
        }
    }

    /// Above it, everything fits and highlights stay apart.
    ///
    /// The range asserted here is the point of the curve. The exponential
    /// shoulder tried first passed a monotonicity check while being white from
    /// twice reference white upwards, so this measures at eight bits — where
    /// the difference either exists or does not — across a range a desktop
    /// actually composites.
    #[test]
    fn above_the_knee_the_range_is_compressed_but_highlights_stay_apart() {
        let mut last = rolled_off(KNEE, KNEE);
        for x in [0.8, 1.0, 1.5, 2.0, 4.0, 8.0, 16.0, 32.0] {
            let y = rolled_off(x, KNEE);
            assert!(y < 1.0, "{x} reached {y}, and white has to stay reachable");
            assert!(y > last, "{x} did not rise above the value before it — a flat patch");
            last = y;
        }

        // Eight bits is what survives to the encoder, so that is where a
        // highlight either keeps its shape or does not.
        let level = |x: f32| (encode(rolled_off(x, KNEE)) * 255.0).round();
        for (dim, bright) in [(1.0, 2.0), (2.0, 4.0), (4.0, 8.0), (8.0, 16.0)] {
            assert!(
                level(bright) > level(dim),
                "{dim}x and {bright}x reference white both encode to {}, so the \
                 highlight between them is a flat patch",
                level(dim)
            );
        }
    }

    /// The curve has no crease where it takes over.
    ///
    /// Matching the value alone would leave a visible edge across any gradient
    /// crossing the knee, and a sky is exactly such a gradient. Measured as a
    /// slope either side rather than asserted from the algebra.
    #[test]
    fn the_curve_meets_the_line_smoothly() {
        let h = 1e-4;
        let below = (rolled_off(KNEE, KNEE) - rolled_off(KNEE - h, KNEE)) / h;
        let above = (rolled_off(KNEE + h, KNEE) - rolled_off(KNEE, KNEE)) / h;
        assert!(
            (below - above).abs() < 0.01,
            "slope is {below} below the knee and {above} above it, which is a crease"
        );
    }

    /// Colours outside Rec.709 arrive as negative light and must not wrap.
    #[test]
    fn negative_light_becomes_black_rather_than_bright() {
        assert_eq!(rolled_off(-0.5, KNEE), 0.0);
        assert_eq!(rolled_off(-1e9, KNEE), 0.0);
    }

    /// The shader must compute what [`rolled_off`] says it computes.
    ///
    /// Two independent implementations of one specification, checked against
    /// each other on the hardware that will run it. This is the only way the
    /// HLSL gets exercised at all without an HDR display: the shader neither
    /// knows nor cares that these floating-point pixels came from a synthetic
    /// texture rather than a duplicated desktop.
    ///
    /// Both source paths are covered, because they differ in a way that a
    /// desktop shows and a test otherwise would not: a duplicated surface
    /// usually cannot be bound as a shader resource, so it is copied first, and
    /// that copy is a whole second way for the plumbing to be wrong.
    #[test]
    fn the_shader_computes_the_same_curve_as_the_rust_it_mirrors() {
        let Some((device, context)) = test_device() else {
            eprintln!("no D3D11 device — skipping");
            return;
        };

        // All exactly representable in half precision, so the comparison is
        // against the value meant rather than the nearest one storable.
        let inputs: [f32; 12] =
            [-0.5, 0.0, 0.25, 0.5, 0.75, 0.875, 1.0, 1.5, 2.0, 4.0, 8.0, 16.0];
        let width = inputs.len() as u16;

        for directly_bindable in [true, false] {
            let source = float_texture(&device, &inputs, directly_bindable);
            let mut path = ShaderPath::new(&device, &context, width, 1)
                .expect("the shader path should build on any D3D11 device");
            let target = path.convert(&source).expect("convert");
            let pixels = read_back(&device, &context, &target, width);

            for (i, &x) in inputs.iter().enumerate() {
                let expected = (encode(rolled_off(x, KNEE)) * 255.0).round();
                // BGRA: all three channels carry the same value here, so a
                // channel swap would show up as well.
                for (channel, offset) in [("blue", 0), ("green", 1), ("red", 2)] {
                    let got = f32::from(pixels[i * 4 + offset]);
                    assert!(
                        (got - expected).abs() <= 1.0,
                        "scRGB {x} came back as {got} in {channel}, and the curve says \
                         {expected} (bound directly: {directly_bindable})"
                    );
                }
                assert_eq!(pixels[i * 4 + 3], 255, "alpha must be opaque");
            }
        }
    }

    /// A device, or `None` on a machine with no D3D11 at all.
    fn test_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
        };

        let mut device = None;
        let mut context = None;
        // SAFETY: out-parameters are checked; no other arguments are borrowed.
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
                Some(&mut context),
            )
            .ok()?;
        }
        Some((device?, context?))
    }

    /// One row of scRGB, each pixel a grey at the given linear intensity.
    fn float_texture(
        device: &ID3D11Device,
        values: &[f32],
        bindable: bool,
    ) -> ID3D11Texture2D {
        let pixels: Vec<u16> =
            values.iter().flat_map(|&v| [half(v), half(v), half(v), half(1.0)]).collect();

        let desc = D3D11_TEXTURE2D_DESC {
            Width: values.len() as u32,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            // False is the interesting case: it is what a duplicated desktop
            // looks like, and it sends the converter down its copying path.
            BindFlags: if bindable { D3D11_BIND_SHADER_RESOURCE.0 as u32 } else { 0 },
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: (values.len() * 8) as u32,
            ..Default::default()
        };

        let mut texture = None;
        // SAFETY: descriptor and initial data are initialised and outlive the
        // call; the out-parameter is checked.
        unsafe {
            device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)).expect("source");
        }
        texture.expect("source texture")
    }

    /// The converted row, as BGRA bytes.
    fn read_back(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        target: &ID3D11Texture2D,
        width: u16,
    ) -> Vec<u8> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
        };

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width as u32,
            Height: 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };

        let mut staging = None;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture matches the target's format and size, and
        // is unmapped before the borrow of its pixels ends.
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut staging)).expect("staging");
            let staging = staging.expect("staging texture");
            context.CopyResource(&staging, target);
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).expect("map");
            let row = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                width as usize * 4,
            )
            .to_vec();
            context.Unmap(&staging, 0);
            row
        }
    }

    /// `f32` to half precision, for the values this test uses.
    ///
    /// Every one of them is a small power of two or a sum of two, so it is
    /// normal in half precision and the mantissa loses nothing. Anything else
    /// would need rounding this does not do.
    fn half(x: f32) -> u16 {
        if x == 0.0 {
            return 0;
        }
        let bits = x.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        assert!((1..31).contains(&exponent), "{x} is not a normal half");
        let mantissa = ((bits & 0x7f_ffff) >> 13) as u16;
        sign | ((exponent as u16) << 10) | mantissa
    }

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
