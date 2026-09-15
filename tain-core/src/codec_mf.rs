//! Hardware H.264 encoding via Media Foundation.
//!
//! This is what makes native-resolution streaming viable. The software encoder
//! needs ~24 ms for a 2560x1600 frame even with SIMD and multi-slice threading;
//! a GPU encoder does the same work in single-digit milliseconds and leaves the
//! CPU free for everything else.
//!
//! ## Why Media Foundation rather than NVENC/AMF directly
//!
//! Vendor SDKs are faster to get working and useless to everyone with the wrong
//! GPU. Media Foundation enumerates whatever hardware encoder the machine
//! actually has — NVIDIA, AMD, Intel — behind one interface that ships with
//! Windows. For an app people install on hardware you have never seen, "works
//! on all of them" beats "optimal on one of them".
//!
//! ## Async transforms
//!
//! Hardware encoder MFTs are almost always **asynchronous**: you do not call
//! `ProcessInput` whenever you like, you wait for the transform to ask. The
//! event loop below (`METransformNeedInput` / `METransformHaveOutput`) is not
//! optional ceremony — a synchronous drain deadlocks against a real hardware
//! MFT. Async transforms also have to be explicitly unlocked before use, which
//! is why `MF_TRANSFORM_ASYNC_UNLOCK` is set before anything else happens.
//!
//! ## Two things that make the output undecodable
//!
//! Both are invisible from this side — the encoder reports success and produces
//! plausible bytes — and both broke every frame of every session until found:
//!
//! - The profile must be **Baseline**, not Main. OpenH264, which the guest
//!   decodes with, supports Constrained Baseline only.
//! - **Access Unit Delimiters must be stripped.** AMD's MFT emits one per frame
//!   and OpenH264 rejects the access unit that contains it. See
//!   [`strip_unsupported_nals`].
//!
//! The test that guards this asserts a *picture out of the decoder*, not bytes
//! out of the encoder. An earlier version checked only the latter and passed
//! throughout.
//!
//! ## Input format
//!
//! Frames go in as NV12 in system memory. A zero-copy path — handing the
//! encoder the D3D11 texture that Desktop Duplication already produced — would
//! additionally save the readback and the colour conversion, roughly 14 ms at
//! 1600p, and is the obvious next step. It needs the MFT bound to a D3D11
//! device manager and the capture backend to surrender its texture, so it is a
//! larger change than this and deliberately not bundled with it.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use std::sync::Once;
use tracing::*;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;

use super::encoder::{EncodeConfig, RateControl};
use super::screen_capture::PixelFormat;

/// How long to wait for a wedged transform before giving up on the session.
///
/// Generous — a busy GPU can legitimately take a while — but finite, because the
/// alternative is a frozen picture with nothing in the log.
const ENCODER_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long to wait by yielding rather than sleeping.
///
/// **`thread::sleep` is the wrong tool for a sub-millisecond wait on Windows.**
/// The default timer resolution is 15.6 ms, so a 200 µs sleep really sleeps for
/// milliseconds — measured here as a jump from 6 ms to 25 ms per frame, four
/// times the budget, caused entirely by the waiting strategy. Yielding gives the
/// core up at microsecond granularity, which is what a wait this short needs.
///
/// After this much the encoder is not merely busy, so the loop switches to
/// sleeping and stops burning a core while it waits to time out.
const ENCODER_SPIN_LIMIT: std::time::Duration = std::time::Duration::from_millis(20);

/// Sleep granularity once past [`ENCODER_SPIN_LIMIT`], where latency no longer
/// matters because the frame is already late.
const ENCODER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

/// One-time Media Foundation startup. Repeated `MFStartup` calls are reference
/// counted, but doing it once keeps the lifetime obvious.
static MF_INIT: Once = Once::new();

pub(crate) fn ensure_mf_started() {
    MF_INIT.call_once(|| {
        // SAFETY: MFStartup with the current version; no pointers involved.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            warn!("MFStartup failed: {e}");
        }
    });
}

/// What a hardware encoder search turned up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// Friendly name reported by the transform, e.g. "AMD H.264 Encoder MFT".
    pub name: String,
    /// Whether this is a hardware transform rather than a software fallback.
    pub hardware: bool,
}

/// The answer to "does this machine have a GPU encoder", worked out once.
///
/// See [`probe`] for why this is cached and not simply called.
static HARDWARE_ENCODER: std::sync::OnceLock<Option<EncoderInfo>> = std::sync::OnceLock::new();

/// Look for a hardware H.264 encoder without committing to using it.
///
/// Used by the settings UI to say what this machine actually has, and by
/// `create_encoder` to decide whether the hardware path is available at all.
///
/// **This is not the cheap lookup it reads as.** Answering honestly means
/// enumerating the transforms *and activating one*, which loads the vendor's
/// driver and allocates a real encode session — measured at **188 ms** on an
/// AMD card. The settings panel called it up to twice per frame to label a
/// line of text, which is 376 ms of work per frame: the whole window, and every
/// dropdown in it, redrew at under 3 fps.
///
/// So it is worked out once and kept. The set of encoders on a machine does not
/// change while an app is running, short of a driver update or a GPU being
/// hot-plugged — and neither of those needs to be handled here, because the
/// answer is only ever used to *label* things and to choose a starting point.
/// Building the real encoder still enumerates afresh and still falls back if
/// the hardware has gone away, which is where being wrong would actually cost
/// something.
pub fn probe() -> Option<EncoderInfo> {
    HARDWARE_ENCODER
        .get_or_init(|| {
            ensure_mf_started();
            let started = std::time::Instant::now();
            let found = match enumerate(super::encoder::VideoCodec::H264, true, "") {
                // The transform is dropped immediately: activating it is the
                // point, because an MFT that enumerates but will not activate
                // is not an encoder this machine has. Dropping it now also shuts
                // the object down, which is what hands the encode session back —
                // see `ActivatedTransform`.
                Ok(Some((_, name))) => Some(EncoderInfo { name, hardware: true }),
                _ => None,
            };
            debug!("Hardware encoder probe took {:?} — {found:?}", started.elapsed());
            found
        })
        .clone()
}

/// The Media Foundation subtype for a codec this encoder can produce.
///
/// `None` for anything Media Foundation has no encoder category for, which is
/// how [`VideoCodec::H265`] and the rest stay honest: asking for a codec the
/// platform cannot encode has to fail here rather than produce H.264 wearing
/// the wrong label.
fn subtype_for(codec: super::encoder::VideoCodec) -> Option<GUID> {
    use super::encoder::VideoCodec;
    match codec {
        VideoCodec::H264 => Some(MFVideoFormat_H264),
        VideoCodec::H265 => Some(MFVideoFormat_HEVC),
        VideoCodec::Av1 => Some(MFVideoFormat_AV1),
        _ => None,
    }
}

/// Take ownership of the array `MFTEnumEx` allocated.
///
/// # The leak this exists to prevent
///
/// `MFTEnumEx` returns a CoTaskMem array of **AddRef'd** `IMFActivate`
/// pointers. Reading them as `&IMFActivate` out of a borrowed slice and then
/// calling `CoTaskMemFree` — which is what this used to do — releases the
/// array's *memory* and nothing else. Every activate in it kept its reference
/// forever.
///
/// That is not a slow drip of a few bytes. An activation object holds the
/// object it created, so each enumeration that activated a transform pinned one
/// **GPU encode session** for the life of the process. Every settings change
/// enumerates. After a handful of them the driver refuses to hand out another,
/// `SetOutputType` starts failing, and from that moment every new session dies
/// at its first frame — until the app is restarted, which is exactly what a
/// leak looks like from the outside.
///
/// Moving each entry out transfers the reference, so dropping the returned
/// vector releases what this owes.
pub(crate) unsafe fn owned_activates(
    ptr: *mut Option<IMFActivate>,
    count: u32,
) -> Vec<IMFActivate> {
    let mut owned = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        // SAFETY: MFTEnumEx guarantees `count` initialised entries, and reading
        // each one moves the reference here rather than copying it.
        if let Some(activate) = unsafe { std::ptr::read(ptr.add(index)) } {
            owned.push(activate);
        }
    }
    // SAFETY: the array was allocated by Media Foundation, and everything it
    // held has been moved out above, so this frees memory that owns nothing.
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(ptr as *const _)) };
    owned
}

/// A transform together with the activation object that produced it.
///
/// # Why they have to stay together
///
/// Releasing the `IMFTransform` is not enough to give a GPU encode session
/// back. The activation object caches the object it created, and Media
/// Foundation documents `ShutdownObject` as the counterpart to
/// `ActivateObject`. Bundling the two means every path that stops using a
/// transform also releases the driver-side session — including the paths that
/// only ever wanted its name.
pub(crate) struct ActivatedTransform {
    pub(crate) transform: IMFTransform,
    activate: IMFActivate,
}

impl Drop for ActivatedTransform {
    fn drop(&mut self) {
        // SAFETY: shutting down an object this activate created, once.
        unsafe {
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Enumerate encoder transforms for one codec, returning the first that activates.
fn enumerate(
    codec: super::encoder::VideoCodec,
    hardware_only: bool,
    prefer: &str,
) -> Result<Option<(ActivatedTransform, String)>> {
    let Some(subtype) = subtype_for(codec) else {
        return Ok(None);
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    let mut flags = MFT_ENUM_FLAG_SORTANDFILTER;
    flags |= if hardware_only {
        MFT_ENUM_FLAG_HARDWARE
    } else {
        MFT_ENUM_FLAG_SYNCMFT
    };

    // SAFETY: MFTEnumEx writes an array of IMFActivate we take ownership of.
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx failed")?;
    }

    if activates.is_null() || count == 0 {
        return Ok(None);
    }

    // SAFETY: MFTEnumEx guarantees `count` initialised entries, and this takes
    // ownership of every reference in them — see `owned_activates`.
    let items = unsafe { owned_activates(activates, count) };

    // # Why the first one is not simply taken
    //
    // A laptop with an integrated and a discrete GPU registers an encoder for
    // each, and the order is the system's to decide. Taking the first meant a
    // machine with a perfectly good NVIDIA encoder silently used the Intel one
    // — which is also the one that stalls on some profiles, so "it picked the
    // other GPU" and "the session froze" arrived as the same report.
    //
    // A preference is matched loosely against the friendly name, because those
    // names are vendor prose: "Intel(R) Quick Sync Video H.264 Encoder MFT",
    // "NVIDIA H.264 Encoder MFT", "AMDh264Encoder".
    let mut ordered: Vec<&IMFActivate> = items.iter().collect();
    if !prefer.is_empty() {
        let wanted = prefer.to_lowercase();
        ordered.sort_by_key(|a| !friendly_name(a).to_lowercase().contains(&wanted));
    }

    let mut found = None;
    for activate in ordered {
        let name = friendly_name(activate);
        // SAFETY: activating a transform we just enumerated.
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                // Debug, not info: the adaptive controller rebuilds the encoder
                // whenever it changes the plan, so this is not a once-per-session
                // event and logging it as one buries everything else.
                debug!("Using video encoder: {name}");
                found = Some((
                    ActivatedTransform { transform, activate: activate.clone() },
                    name,
                ));
                break;
            }
            Err(e) => debug!("Could not activate encoder {name}: {e}"),
        }
    }
    if !prefer.is_empty()
        && let Some((_, name)) = &found
        && !name.to_lowercase().contains(&prefer.to_lowercase())
    {
        // Said out loud: the user asked for one GPU and got another, and a
        // silent substitution here is the difference between "this is slow" and
        // "this is slow *because it is not using the card I chose*".
        info!("Preferred encoder {prefer:?} is not available — using {name}");
    }

    // Everything not chosen is released here; the chosen one is kept alive by
    // the `ActivatedTransform` that now owns a clone of it.
    drop(items);
    Ok(found)
}

/// Every hardware H.264 encoder this machine registers, by name.
///
/// What an encoder picker offers. Enumerated rather than guessed at: the names
/// are vendor prose and differ between driver versions, so a fixed list of
/// "NVIDIA / AMD / Intel" would be wrong on the machine that needed it.
pub fn available_encoders() -> Vec<String> {
    available_encoders_for(super::encoder::VideoCodec::H264)
}

/// The same, for one particular codec — which is how "can this machine encode
/// HEVC at all" is answered without building an encoder to find out.
pub fn available_encoders_for(codec: super::encoder::VideoCodec) -> Vec<String> {
    ensure_mf_started();
    let Some(subtype) = subtype_for(codec) else {
        return Vec::new();
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: read-only enumeration; the array is freed below.
    let ok = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    }
    .is_ok();
    if !ok || activates.is_null() || count == 0 {
        return Vec::new();
    }

    // SAFETY: MFTEnumEx guarantees `count` initialised entries; taking
    // ownership is what releases them — see `owned_activates`.
    let items = unsafe { owned_activates(activates, count) };
    items.iter().map(friendly_name).collect()
}

/// The transform a session for this codec would select, by name.
///
/// # Why this is not [`probe`]
///
/// `probe` answers for H.264 and only H.264, and it was what named the encoder
/// in the log line for *every* codec. So an HEVC session announced itself as
/// "H.265 encoding on the GPU via NVIDIA **H.264** Encoder MFT" — which reads
/// like the wrong stream is being produced, and cost a round of chasing that
/// before the enumeration below showed the stream was fine and the label was
/// not. It also gated HEVC on an H.264 encoder existing, which is not the same
/// question.
///
/// Names only: this enumerates without activating, so it costs nothing and
/// cannot itself hold an encode session. Whether the transform really works is
/// settled where it is built, which already falls back when it does not.
fn encoder_name_for(codec: super::encoder::VideoCodec, prefer: &str) -> Option<String> {
    let names = available_encoders_for(codec);
    if !prefer.is_empty() {
        let wanted = prefer.to_lowercase();
        if let Some(hit) = names.iter().find(|n| n.to_lowercase().contains(&wanted)) {
            return Some(hit.clone());
        }
    }
    names.into_iter().next()
}

pub(crate) fn friendly_name(activate: &IMFActivate) -> String {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    // SAFETY: reading a documented string attribute; a missing one is not fatal.
    // On success the string is a fresh CoTaskMem allocation we own.
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len)
            .is_err()
            || ptr.is_null()
        {
            return "unnamed encoder".to_string();
        }
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, len as usize));
        windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as *const _));
        name
    }
}

/// A Media Foundation H.264 encoder.
pub struct MediaFoundationEncoder {
    /// Kept whole rather than unwrapped to the transform: dropping this is what
    /// releases the GPU encode session, and a bare `IMFTransform` cannot.
    activated: ActivatedTransform,
    events: Option<IMFMediaEventGenerator>,
    width: u16,
    height: u16,
    frame_index: i64,
    frame_duration: i64,
    /// Input requests an async transform has issued that we have not yet
    /// satisfied — see [`Self::pump_async`] for why these must survive the call
    /// that observed them.
    pending_input_requests: u32,
    /// What this transform is actually producing.
    ///
    /// Needed because the bitstream leaving it is not the same shape for every
    /// codec — see [`strip_unsupported_nals`], which is Annex-B H.264 logic and
    /// must not be pointed at anything else.
    codec: super::encoder::VideoCodec,
    pub info: EncoderInfo,
}

// SAFETY: the transform is used from exactly one thread — the session's capture
// pipeline thread — and never shared. The COM objects themselves are
// apartment-agnostic (MTA), which is what MFStartup(NOSOCKET) establishes.
unsafe impl Send for MediaFoundationEncoder {}

impl MediaFoundationEncoder {
    /// Build an encoder for a given frame size, or fail if no hardware transform
    /// can be configured.
    pub fn new(config: &EncodeConfig, width: u16, height: u16) -> Result<Self> {
        ensure_mf_started();

        let codec = config.effective_codec();
        let (activated, name) = enumerate(codec, true, &config.preferred_encoder)?
            .with_context(|| format!("this machine has no hardware {codec} encoder"))?;
        let info = EncoderInfo { name, hardware: true };

        let encoder = Self {
            activated,
            events: None,
            width,
            height,
            frame_index: 0,
            frame_duration: 10_000_000 / config.fps.max(1) as i64,
            pending_input_requests: 0,
            codec,
            info,
        };
        encoder.configure(config)
    }

    fn configure(mut self, config: &EncodeConfig) -> Result<Self> {
        // SAFETY: every call below is a documented IMFTransform/IMFAttributes
        // operation on an object we own.
        unsafe {
            // An async MFT refuses to do anything until it is unlocked. This
            // must happen before types are set, not after.
            let attrs = self.activated.transform.GetAttributes().ok();
            let is_async = attrs
                .as_ref()
                .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok())
                .unwrap_or(0)
                == 1;
            if is_async {
                if let Some(ref attrs) = attrs {
                    attrs
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .context("could not unlock the async encoder")?;
                }
                self.events = self.activated.transform.cast::<IMFMediaEventGenerator>().ok();
            }
            // Low latency: no B-frames, no lookahead. Without this the encoder
            // buffers frames to improve compression, which is exactly the wrong
            // trade for a remote desktop.
            if let Some(ref attrs) = attrs {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            // Output type first — H.264 encoders require it before the input
            // type, and rejecting that order is a common silent failure.
            let output = MFCreateMediaType().context("MFCreateMediaType")?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            let subtype = subtype_for(config.effective_codec())
                .context("this codec has no Media Foundation encoder")?;
            output.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps(config, self.width, self.height))?;
            set_ratio(&output, &MF_MT_FRAME_SIZE, self.width as u32, self.height as u32)?;
            set_ratio(&output, &MF_MT_FRAME_RATE, config.fps.max(1), 1)?;
            set_ratio(&output, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // # Say what the pixels mean, rather than letting each codec guess
            //
            // Capture produces BT.601 limited-range NV12 and the guest converts
            // it back the same way — see `capture_nv12` and `decode_rgba`. The
            // encoder was never told, and for H.264 whatever it assumed happened
            // to match. For HEVC it did not: the same pixels came back visibly
            // darker, because samples encoded as full range were then expanded
            // as if they were limited, crushing everything below 16 to black.
            //
            // Declared on both types so there is nothing left to infer.
            output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
            // # The profile is chosen, not fixed
            //
            // It was hard-coded to Baseline because the guest's CPU fallback
            // decoder cannot read anything else — which is still true and still
            // the default. Baseline is also where **Intel Quick Sync stalls**,
            // so hosts with that encoder need to be able to raise it.
            //
            // See [`H264Profile`] for the trade and who can safely take it.
            // # The profile has to be stated for HEVC too
            //
            // It was left unset, on the reasoning that HEVC's profile constants
            // are a different enumeration and Main is what every encoder
            // produces anyway. The second half of that was wrong.
            //
            // Left to choose, an encoder may pick a **10-bit** profile. Nothing
            // fails: it accepts 8-bit NV12 and emits a Main 10 stream, the
            // decoder returns P010 surfaces, and the colour converter — built
            // for NV12 — reads ten-bit samples as eight-bit ones. The result is
            // a picture that is far too dark and falls apart into blocks, which
            // is exactly what was reported and exactly what it looks like.
            //
            // `eAVEncH265VProfile_Main_420_8` is 1. Named as a literal because
            // the enumeration lives behind a type this call does not take.
            //
            // AV1 takes the same value for the same reason:
            // `eAVEncAV1VProfile_Main_420_8bits` is also 1, and its ten-bit
            // profiles would go wrong in the same way against an NV12 decoder.
            let profile = match config.effective_codec() {
                super::encoder::VideoCodec::H265 | super::encoder::VideoCodec::Av1 => 1,
                _ => config.profile.to_mf(),
            };
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            self.activated.transform
                .SetOutputType(0, &output, 0)
                .with_context(|| {
                    format!("the encoder rejected the {} output type", config.effective_codec())
                })?;

            let input = MFCreateMediaType().context("MFCreateMediaType")?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_ratio(&input, &MF_MT_FRAME_SIZE, self.width as u32, self.height as u32)?;
            set_ratio(&input, &MF_MT_FRAME_RATE, config.fps.max(1), 1)?;
            set_ratio(&input, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // The same declaration on the way in, so the encoder knows what it
            // is being handed as well as what it should produce.
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
            self.activated.transform
                .SetInputType(0, &input, 0)
                .context("the encoder rejected the NV12 input type")?;

            // Rate control and GOP, where the encoder exposes them. These are
            // best-effort: not every vendor MFT implements ICodecAPI fully, and
            // a missing knob is not a reason to refuse to encode.
            self.apply_codec_api(config);

            self.activated.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.activated.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.activated.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(self)
    }

    /// Apply rate control through ICodecAPI, ignoring anything unsupported.
    ///
    /// ## The buffer size is the setting that makes a bitrate limit real
    ///
    /// Asking for a mean bitrate and nothing else is close to advisory. Measured
    /// on an AMD MFT: a 2 Mbit/s request produced 10.8 Mbit/s and a 25 Mbit/s
    /// request produced 51 Mbit/s. The rate controller was averaging over a
    /// window long enough that a second of overshoot was, to it, on target.
    ///
    /// That is not a cosmetic inaccuracy. Bitrate is the adaptive controller's
    /// only real lever against a link that cannot keep up, so an encoder that
    /// ignores it turns a slow link into a **frozen picture**: the controller
    /// backs off, nothing changes, the bounded frame queue stays full, and the
    /// capture thread blocks forever.
    ///
    /// [`CODECAPI_AVEncCommonBufferSize`] is the HRD/VBV buffer — the window the
    /// rate controller is actually allowed to average over. Sized here at a
    /// quarter second, which is short enough that overshoot has to be paid back
    /// before the guest notices and long enough that one detailed frame does not
    /// collapse quality. A peak cap goes alongside it, because CBR without one
    /// still lets individual frames spike far past what the link can carry.
    unsafe fn apply_codec_api(&self, config: &EncodeConfig) {
        let Ok(codec_api) = self.activated.transform.cast::<ICodecAPI>() else {
            warn!("{} exposes no ICodecAPI — bitrate cannot be controlled", self.info.name);
            return;
        };

        // SAFETY (all four closures): passing variants we own to documented
        // setters, and reading back with the matching getter. Every knob here is
        // optional — vendors implement different subsets — so a rejection is
        // dropped rather than failing the session.
        let set = |name: &str, guid: &GUID, variant: windows::core::VARIANT| {
            let applied = unsafe { codec_api.SetValue(guid, &variant) };
            if let Err(e) = applied {
                debug!("{} rejected {name}: {e}", self.info.name);
            }
        };
        let set_u32 = |name: &str, guid: &GUID, value: u32| {
            set(name, guid, windows::core::VARIANT::from(value));
        };

        let mode = match config.rate_control {
            RateControl::Constant => eAVEncCommonRateControlMode_CBR,
            RateControl::Variable => eAVEncCommonRateControlMode_PeakConstrainedVBR,
            RateControl::ConstantQuality => eAVEncCommonRateControlMode_Quality,
        };
        // Mode first: several of the settings below are only meaningful — and on
        // some transforms only accepted — once the mode they belong to is set.
        set_u32("rate control mode", &CODECAPI_AVEncCommonRateControlMode, mode.0 as u32);

        let bitrate = bitrate_bps(config, self.width, self.height);
        set_u32("mean bitrate", &CODECAPI_AVEncCommonMeanBitRate, bitrate);
        if config.rate_control != RateControl::ConstantQuality {
            set_u32("buffer size", &CODECAPI_AVEncCommonBufferSize, bitrate / 4);
            // Headroom over the mean rather than equal to it: a hard peak at the
            // mean makes every keyframe a quality cliff.
            set_u32(
                "peak bitrate",
                &CODECAPI_AVEncCommonMaxBitRate,
                bitrate.saturating_mul(3) / 2,
            );
        } else {
            set_u32("quality", &CODECAPI_AVEncCommonQuality, config.quality.min(100) as u32);
        }

        // A GOP of 0 means "only on request", which is what an interactive
        // stream wants — keyframes are large and spike latency.
        set_u32("GOP size", &CODECAPI_AVEncMPVGOPSize, config.keyframe_interval);
        // Boolean, not UI4. Handing it the wrong variant type is silently
        // rejected, which is how it went unnoticed that low-latency mode was
        // never actually on.
        set("low latency", &CODECAPI_AVEncCommonLowLatency, windows::core::VARIANT::from(true));

        // Report what the transform really accepted. Vendors differ, and a
        // stream misbehaving because a knob was ignored is otherwise invisible.
        let readback = |guid: &GUID| -> Option<u32> {
            unsafe { codec_api.GetValue(guid).ok() }.and_then(|v| u32::try_from(&v).ok())
        };
        debug!(
            "{}: rate control mode={:?}, mean={:?} bps, buffer={:?} bits (asked {bitrate})",
            self.info.name,
            readback(&CODECAPI_AVEncCommonRateControlMode),
            readback(&CODECAPI_AVEncCommonMeanBitRate),
            readback(&CODECAPI_AVEncCommonBufferSize),
        );
    }

    /// Change the bitrate on a running transform.
    ///
    /// **This is why the adaptive controller does not rebuild the encoder.**
    /// Rebuilding means enumerating transforms, activating one and negotiating
    /// media types — hundreds of milliseconds, on the capture thread, landing
    /// squarely in the frame it happened to interrupt. That frame then reports
    /// as "slow encoding", which makes the controller adapt again, which
    /// rebuilds again: the stream spends its life renegotiating and the log
    /// fills with adaptations that are chasing their own cost.
    ///
    /// Bitrate is the one thing the controller changes often, and every vendor
    /// MFT accepts it live — verified by reading it back.
    fn set_bitrate(&self, bps: u32) -> Result<()> {
        // SAFETY: documented ICodecAPI setters on a transform we own.
        unsafe {
            let codec_api = self
                .activated
                .transform
                .cast::<ICodecAPI>()
                .context("this transform has no ICodecAPI")?;
            let set = |guid: &GUID, value: u32| -> Result<()> {
                codec_api
                    .SetValue(guid, &windows::core::VARIANT::from(value))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            };
            set(&CODECAPI_AVEncCommonMeanBitRate, bps)?;
            // The buffer and peak have to move with it, or the window the rate
            // controller averages over no longer matches the rate it is holding.
            let _ = set(&CODECAPI_AVEncCommonBufferSize, bps / 4);
            let _ = set(&CODECAPI_AVEncCommonMaxBitRate, bps.saturating_mul(3) / 2);
        }
        Ok(())
    }

    /// Encode one frame, returning Annex-B bytes and whether it is a keyframe.
    ///
    /// `format` says what `pixels` holds. NV12 is what the transform wants, so
    /// a caller that already has it — the GPU capture path — skips the
    /// conversion entirely rather than converting back and forth.
    pub fn encode(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<(Vec<u8>, bool)> {
        if (width, height) != (self.width, self.height) {
            anyhow::bail!(
                "encoder was built for {}x{}, got {width}x{height}",
                self.width,
                self.height
            );
        }
        let expected = format.frame_bytes(width, height);
        if pixels.len() < expected {
            anyhow::bail!("{format:?} buffer too small: {} < {expected}", pixels.len());
        }

        let sample = self.make_sample(&pixels[..expected], width, height, format)?;
        self.pump(sample)
    }

    /// Build an IMFSample holding this frame as NV12.
    ///
    /// The conversion writes **directly into the media buffer**. Converting into
    /// a staging `Vec` and copying would move 6 MB twice per frame at 1600p, for
    /// nothing — the transform is the only reader either way. NV12 input is
    /// copied straight in, which is the same reasoning taken one step further.
    fn make_sample(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<IMFSample> {
        let len = nv12_len(width, height);
        // SAFETY: allocating a media buffer and filling the region it hands
        // back. `Lock` reports the capacity, which is checked before writing.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len as u32)?;
            let mut dst = std::ptr::null_mut();
            let mut max = 0u32;
            buffer.Lock(&mut dst, Some(&mut max), None)?;
            if (max as usize) < len {
                let _ = buffer.Unlock();
                anyhow::bail!("media buffer is {max} bytes, need {len}");
            }
            let out = std::slice::from_raw_parts_mut(dst, len);
            match format {
                PixelFormat::Bgra => bgra_to_nv12_into(pixels, width, height, out),
                PixelFormat::Nv12 => out.copy_from_slice(&pixels[..len]),
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(len as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frame_index * self.frame_duration)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.frame_index += 1;
            Ok(sample)
        }
    }

    /// Feed one sample and collect whatever comes out.
    ///
    /// For an async transform this drives the event loop until the encoder has
    /// both taken the input and produced its output.
    fn pump(&mut self, sample: IMFSample) -> Result<(Vec<u8>, bool)> {
        match self.events.clone() {
            Some(events) => self.pump_async(&events, sample),
            None => self.pump_sync(sample),
        }
    }

    fn pump_sync(&mut self, sample: IMFSample) -> Result<(Vec<u8>, bool)> {
        // SAFETY: documented synchronous MFT flow.
        unsafe {
            self.activated.transform.ProcessInput(0, &sample, 0)?;
        }

        let mut out = Vec::new();
        let mut keyframe = false;
        while let Some((data, key)) = self.process_output()? {
            out.extend_from_slice(&self.clean(&data));
            keyframe |= key;
        }
        Ok((out, keyframe))
    }

    /// Async MFT flow: feed one frame, take whatever output is already ready.
    ///
    /// The shape here is dictated by two facts that pull in opposite directions.
    /// An async transform will not accept input until it has asked for it, so we
    /// must block until a `METransformNeedInput` arrives. But it is also free to
    /// hold a frame back — hardware encoders typically emit frame N's bitstream
    /// only once frame N+1 has gone in — so blocking for output after feeding
    /// would deadlock the capture thread on a frame that will never come.
    ///
    /// So: block only for permission to feed, then drain non-blocking. That
    /// leaves the first frame or two producing nothing, which callers already
    /// handle as "no packet this tick", and costs one frame of pipeline latency
    /// in the steady state.
    ///
    /// `pending_input_requests` carries surplus requests across calls. Dropping
    /// them instead would desynchronise us from the transform: it would be
    /// waiting for us to feed against a request we had thrown away, and the next
    /// blocking wait would never return.
    fn pump_async(
        &mut self,
        events: &IMFMediaEventGenerator,
        sample: IMFSample,
    ) -> Result<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        let mut keyframe = false;

        // Wait until the transform is willing to take a frame — but not forever.
        //
        // `GetEvent` has no timeout, and a blocking call here is on the capture
        // thread: if a driver wedges, or stops issuing requests for a reason we
        // cannot see, the session does not fail, it *freezes*, with no log line
        // and nothing to distinguish it from a slow network. Polling costs a
        // fraction of a millisecond of added latency and turns that into an
        // error someone can act on.
        let started = std::time::Instant::now();
        while self.pending_input_requests == 0 {
            // SAFETY: non-blocking poll of a generator we own.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event, &mut out, &mut keyframe)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    let waited = started.elapsed();
                    if waited >= ENCODER_STALL_TIMEOUT {
                        anyhow::bail!(
                            "{} stopped asking for frames — the encoder has stalled",
                            self.info.name
                        );
                    }
                    if waited < ENCODER_SPIN_LIMIT {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(ENCODER_POLL_INTERVAL);
                    }
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("waiting for the encoder to request input: {e}"))
                }
            }
        }

        self.pending_input_requests -= 1;
        // SAFETY: feeding the sample against a request the transform issued.
        unsafe { self.activated.transform.ProcessInput(0, &sample, 0) }
            .context("the encoder rejected a frame")?;

        // Anything already queued is ours to take; anything not, we leave for
        // the next frame rather than waiting on it.
        loop {
            // SAFETY: non-blocking poll of the same generator.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event, &mut out, &mut keyframe)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(anyhow::anyhow!("encoder event queue failed: {e}")),
            }
        }

        Ok((out, keyframe))
    }

    /// Apply one transform event: either a request for input, or output to take.
    fn handle_event(
        &mut self,
        event: &IMFMediaEvent,
        out: &mut Vec<u8>,
        keyframe: &mut bool,
    ) -> Result<()> {
        // SAFETY: reading the type of an event we hold.
        let kind = unsafe { event.GetType() }?;

        if kind == METransformNeedInput.0 as u32 {
            self.pending_input_requests += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            // Exactly one call per event — see `process_output`.
            if let Some((data, key)) = self.process_output()? {
                let data = self.clean(&data);
                // A frame can arrive as several output samples; concatenating
                // keeps the Annex-B stream contiguous rather than dropping
                // earlier NALs.
                out.extend_from_slice(&data);
                *keyframe |= key;
            }
        }
        Ok(())
    }

    /// Tidy one packet on the way out, where tidying it means anything.
    ///
    /// See [`needs_annex_b_cleanup`]: the cleanup is H.264 reasoning, and the
    /// other codecs are handed on untouched.
    fn clean(&self, data: &[u8]) -> Vec<u8> {
        if needs_annex_b_cleanup(self.codec) {
            strip_unsupported_nals(data)
        } else {
            data.to_vec()
        }
    }

    /// Take one output sample, if the transform has one ready.
    ///
    /// Deliberately not a loop. A synchronous MFT answers a speculative call
    /// with `MF_E_TRANSFORM_NEED_MORE_INPUT`, so looping until that is the
    /// normal pattern — but an **async** MFT treats a call it did not invite as
    /// a protocol violation and returns `E_UNEXPECTED`, which is indistinguish-
    /// able from a real failure. So the caller decides: [`Self::pump_sync`]
    /// loops until this returns `None`, and the async path calls it once per
    /// `METransformHaveOutput`.
    fn process_output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        // The transform allocates its own samples for hardware MFTs, so the
        // sample field goes in empty and comes back filled.
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: `buffers` is initialised above and outlives the call; the two
        // ManuallyDrop fields are taken exactly once each afterwards, which is
        // what releases whatever the transform put there.
        let (result, sample) = unsafe {
            let result = self.activated.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            // Release any event object the transform attached.
            drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
            (result, sample)
        };

        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Renegotiation is only expected on a format change, which we
                // never do mid-stream; treat it as end of output.
                return Ok(None);
            }
            Err(e) => return Err(anyhow::anyhow!("ProcessOutput failed: {e}")),
        }

        let Some(sample) = sample else { return Ok(None) };
        let mut out = Vec::new();
        // SAFETY: reading the sample the transform just produced.
        let keyframe = unsafe {
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            out.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
            buffer.Unlock()?;
            keyframe
        };
        Ok(Some((out, keyframe)))
    }
}

impl Drop for MediaFoundationEncoder {
    /// Hand the encode session back, in the order the driver expects.
    ///
    /// # Why the order is the whole of this
    ///
    /// `ShutdownObject` — which runs when `activated` drops — blocked forever,
    /// roughly once in every ten runs of the test suite, and this is why. Two
    /// things were wrong, and both are about what the transform is still
    /// holding when it is asked to shut down.
    ///
    /// **Nothing flushed.** `encode_frame` deliberately takes only the output
    /// already queued and leaves the rest for the next frame, so an encoder
    /// dropped mid-stream leaves the transform holding samples it has finished
    /// with and signalled, and nobody ever collects. `MFT_MESSAGE_COMMAND_FLUSH`
    /// is what says "abandon that", and it is the counterpart to the flush this
    /// encoder already sends when it starts streaming.
    ///
    /// **The event generator outlived the transform.** Rust drops fields in
    /// declaration order, and `activated` is declared before `events` — so
    /// `ShutdownObject` ran while this struct still held a live reference into
    /// the object being shut down. That is the deadlock: a reference count the
    /// driver waits on, held by the thread doing the waiting.
    ///
    /// So: flush, say the stream is over, let go of the event generator, and
    /// only then let `activated` release the session. See
    /// [`crate::watchdog`] for how this was found, and the entry in
    /// `docs/decisions.md`.
    fn drop(&mut self) {
        // SAFETY: messages to a transform we own, on the thread that owns it.
        unsafe {
            let _ = self.activated.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.activated.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.activated.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        // Explicit, and not a tidy-up: this reference has to be gone before
        // `activated` drops, and relying on field order to arrange that is what
        // went wrong in the first place.
        self.events = None;
    }
}

// ── VideoEncoder adapter ─────────────────────────────────────

/// Whichever H.264 encoder is actually running for a given frame size.
///
/// Boxed because the two are very different sizes — the software encoder
/// carries its plane buffers inline — and only one exists per session, so the
/// indirection costs a pointer hop per frame against a much larger enum.
enum Backend {
    Hardware(Box<MediaFoundationEncoder>),
    Software(Box<super::codec_h264::H264Encoder>),
}

fn encode_with(
    backend: &mut Backend,
    pixels: &[u8],
    width: u16,
    height: u16,
    format: PixelFormat,
) -> Result<(Vec<u8>, bool)> {
    match backend {
        Backend::Hardware(enc) => enc.encode(pixels, width, height, format),
        // # Both formats, because this is the path that saves a session
        //
        // Reaching here with NV12 means the hardware encoder was negotiated and
        // then dropped mid-session. This used to refuse, on the reasoning that
        // the capture side would not let that happen. It does happen — a driver
        // that stops accepting its own output type gets here — and refusing
        // turned one hiccup into a dead session, and then into a host that
        // could not be connected to at all until it was restarted.
        Backend::Software(enc) => match format {
            PixelFormat::Bgra => enc.encode(pixels, width, height),
            PixelFormat::Nv12 => enc.encode_nv12(pixels, width, height),
        },
    }
}

/// How many times a hardware transform may be rebuilt after failing mid-stream
/// **without encoding anything in between**.
///
/// See the restart path in [`HardwareH264Encoder::encode_frame`].
const MAX_ENCODER_RESTARTS: u32 = 3;

/// Frames a rebuilt transform must produce before the restart budget refills.
///
/// # Why the budget is not a lifetime count
///
/// It was, and a session died for it. Every display switch changes the frame
/// size, which rebuilds the transform, and AMD's AV1 MFT has been seen to stall
/// on the first frame after a rebuild. Three switches over half a minute — each
/// one followed by hundreds of perfectly good frames — exhausted a budget meant
/// for an encoder that cannot encode at all, and the session ended with
/// "the encoder has stalled" while the encoder was working.
///
/// What that budget is really trying to say is *this transform never recovers*.
/// So measure recovery: a restart that goes on to produce a run of frames
/// worked, and the next stall is a new problem rather than the third act of the
/// old one. Half a second at sixty frames a second, which a genuinely wedged
/// transform never reaches — it stalls again inside
/// [`ENCODER_STALL_TIMEOUT`] with nothing emitted.
const FRAMES_TO_FORGIVE_A_RESTART: u32 = 30;

/// A hardware transform's allowance for restarting itself after a stall.
///
/// A count with a memory. Split out from the encoder because the bug it
/// replaces was in the counting rather than the encoding, and everything else
/// on that path needs a GPU to reach.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RestartBudget {
    spent: u32,
    frames_since: u32,
}

impl RestartBudget {
    /// Whether there is room for another restart.
    fn has_room(self) -> bool {
        self.spent < MAX_ENCODER_RESTARTS
    }

    /// Record a restart, returning how many have now been spent unrecovered.
    fn spend(&mut self) -> u32 {
        self.spent += 1;
        self.frames_since = 0;
        self.spent
    }

    /// Record a frame the transform actually produced.
    ///
    /// Returns true at the moment the budget refills, and only then, so the
    /// caller can say so once instead of on every frame afterwards.
    fn encoded_a_frame(&mut self) -> bool {
        self.frames_since = self.frames_since.saturating_add(1);
        if self.spent > 0 && self.frames_since >= FRAMES_TO_FORGIVE_A_RESTART {
            self.spent = 0;
            return true;
        }
        false
    }

    /// Forget everything: what is in the slot now is a different transform.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// [`MediaFoundationEncoder`] behind the pipeline's [`VideoEncoder`] interface.
///
/// The transform is built on the first frame and rebuilt if the capture size
/// changes, matching the software encoder — capture, not configuration, is the
/// authority on frame size, and the adaptive controller changes it at runtime.
///
/// ## Why this falls back per size rather than once at startup
///
/// A hardware encoder is not simply present or absent. It has a minimum frame
/// size (which a heavily downscaled stream can fall under), and — more
/// importantly for this app — a cap on **concurrent encode sessions**. Several
/// guests connected at once each build an encoder, and on consumer hardware the
/// third or fourth will be refused while the first three work fine. Treating
/// that as a fatal error would drop a guest for a reason they cannot act on, so
/// the overflow session quietly encodes on the CPU instead.
///
/// `allow_fallback` is false when the user explicitly chose the Hardware
/// backend, because then a silent downgrade is the wrong answer.
pub struct HardwareH264Encoder {
    config: EncodeConfig,
    allow_fallback: bool,
    /// `Mutex` rather than a plain field because [`VideoEncoder`] requires
    /// `Sync` and a Media Foundation transform is single-threaded. It is only
    /// ever touched from the capture thread, so this never contends.
    inner: std::sync::Mutex<Option<(Backend, u16, u16)>>,
    sequence_counter: std::sync::Mutex<u32>,
    /// Hardware transforms rebuilt after a mid-stream failure. Needs no lock —
    /// encoding takes `&mut self`.
    budget: RestartBudget,
    /// Whether the current transform has yet to emit a packet.
    ///
    /// A transform's first output is an IDR, so while this is true a keyframe
    /// has already been asked for and rebuilding to get one is pure churn —
    /// which is what [`Self::request_keyframe`] uses it for.
    fresh: bool,
    name: String,
}

impl HardwareH264Encoder {
    /// Fail here, not on the first frame, if this machine has no hardware
    /// encoder at all — the caller can then fall back before a session starts.
    ///
    /// `allow_fallback` lets a per-size or per-session hardware failure drop to
    /// the CPU encoder rather than ending the session.
    pub fn new(config: &EncodeConfig, allow_fallback: bool) -> Result<Self> {
        let codec = config.effective_codec();
        let name = encoder_name_for(codec, &config.preferred_encoder).with_context(|| {
            format!("no hardware {codec} encoder is available on this machine")
        })?;
        Ok(Self {
            config: config.clone(),
            allow_fallback,
            inner: std::sync::Mutex::new(None),
            sequence_counter: std::sync::Mutex::new(0),
            budget: RestartBudget::default(),
            fresh: true,
            name,
        })
    }

    /// Friendly name of the transform in use, for the UI and logs.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the frames coming out right now are GPU-encoded.
    ///
    /// Not the same as "a hardware encoder exists" — see the fallback note on
    /// the type. The bottleneck display uses this to explain a slow stream.
    pub fn is_using_hardware(&self) -> bool {
        matches!(
            self.inner.lock().unwrap().as_ref(),
            Some((Backend::Hardware(_), _, _))
        )
    }

    fn build(&self, width: u16, height: u16) -> Result<Backend> {
        match MediaFoundationEncoder::new(&self.config, width, height) {
            Ok(encoder) => Ok(Backend::Hardware(Box::new(encoder))),
            Err(e) if self.allow_fallback => {
                warn!(
                    "{} could not encode {width}x{height} ({e}) — this stream falls back to the CPU",
                    self.name
                );
                Ok(Backend::Software(Box::new(super::codec_h264::H264Encoder::new(
                    &self.config,
                )?)))
            }
            Err(e) => Err(e),
        }
    }
}

impl super::encoder::VideoEncoder for HardwareH264Encoder {
    /// Rebuild on the next frame, which is what produces an IDR.
    ///
    /// Not `CODECAPI_AVEncVideoForceKeyFrame`: vendors disagree about whether it
    /// applies to the next input or the one after, and about whether setting it
    /// on a running transform is honoured at all. A rebuild is unambiguous on
    /// every backend.
    ///
    /// # Why a transform that has emitted nothing is left alone
    ///
    /// This used to rebuild unconditionally, on the reasoning that a guest's
    /// decoder failing should be approximately never. Switching displays made it
    /// routine: the frame size changes under the guest, its decoder fails, it
    /// asks for a keyframe — and the host was tearing the transform down twice
    /// within a few tens of milliseconds, once for the request and once for the
    /// new size. Four rebuilds in two seconds is what a run of switches actually
    /// produced, and AMD's AV1 transform stalls when driven like that.
    ///
    /// A transform's first packet is an IDR. So while the current one has yet to
    /// emit anything, the keyframe is already on its way and a rebuild buys
    /// nothing but the stall.
    fn request_keyframe(&mut self) {
        if self.fresh {
            debug!(
                "{}: a guest asked for a keyframe — the encoder is about to send one anyway",
                self.name
            );
            return;
        }
        warn!("{}: a guest asked for a keyframe — rebuilding the encoder", self.name);
        *self.inner.lock().unwrap() = None;
        self.fresh = true;
    }

    /// NV12, so the capture side can convert on the GPU and skip the readback
    /// of four bytes per pixel — see [`super::capture_nv12`].
    fn preferred_format(&self) -> PixelFormat {
        PixelFormat::Nv12
    }

    /// AV1 needs its width on the superblock grid; the rest take anything even.
    ///
    /// Measured, not read off a datasheet — see
    /// [`super::encoder::VideoEncoder::size_granularity`] for what an AV1
    /// transform handed an off-grid size actually does, which is nothing at all
    /// while reporting success.
    ///
    /// Applied to every AV1 transform rather than only AMD's. The constraint is
    /// the codec's own geometry, sixty-four being the superblock, so a vendor
    /// that does not need it loses at most sixty-three columns of a desktop —
    /// against a session that ends two seconds in on a vendor that does.
    fn size_granularity(&self) -> (u16, u16) {
        match self.config.effective_codec() {
            super::encoder::VideoCodec::Av1 => (64, 16),
            _ => (2, 2),
        }
    }

    fn encode_frame(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        format: PixelFormat,
    ) -> Result<super::encoder::EncodedFrame> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            anyhow::bail!("H.264 needs even dimensions, got {width}x{height}");
        }

        let mut slot = self.inner.lock().unwrap();
        if !matches!(slot.as_ref(), Some((_, w, h)) if (*w, *h) == (width, height)) {
            // Drop the old transform before building the new one: encode
            // sessions are a limited resource, and holding both could exhaust
            // them on exactly the hardware where that matters.
            *slot = None;
            *slot = Some((self.build(width, height)?, width, height));
            self.fresh = true;
        }

        let was_hardware = matches!(slot.as_ref(), Some((Backend::Hardware(_), _, _)));
        let mut outcome = encode_with(&mut slot.as_mut().expect("built above").0, pixels, width, height, format);

        // # A wedged transform is not a dead session
        //
        // Intel Quick Sync has been seen to negotiate the type, begin streaming
        // and then stop asking for frames — the stall that
        // [`ENCODER_STALL_TIMEOUT`] catches. Propagating that ended the session
        // outright, so one driver hiccup cost the whole call.
        //
        // A fresh transform usually comes back, and the budget for trying is
        // spent per *unrecovered* failure — see [`FRAMES_TO_FORGIVE_A_RESTART`].
        if let Err(e) = &outcome
            && was_hardware
            && self.budget.has_room()
        {
            let spent = self.budget.spend();
            warn!(
                "{} failed mid-stream ({e}) — rebuilding it \
                 (restart {spent} of {MAX_ENCODER_RESTARTS})",
                self.name
            );
            *slot = None;
            *slot = Some((self.build(width, height)?, width, height));
            self.fresh = true;
            outcome = encode_with(
                &mut slot.as_mut().expect("rebuilt above").0,
                pixels,
                width,
                height,
                format,
            );
        }

        // # The last resort is the CPU, not the end of the session
        //
        // A transform that has used up its restarts is out of ways to fix
        // itself, but the session still has one: the same software encoder a
        // machine with no hardware encoder at all would be using. It is slower
        // and the adaptive controller will scale the picture down to pay for it,
        // which is a stream someone can still work in — and the alternative here
        // is hanging up on them.
        //
        // Not when the user chose the hardware backend by name. There a silent
        // downgrade hides the very thing they asked to be told about.
        if outcome.is_err() && was_hardware && self.allow_fallback {
            let failure = outcome.as_ref().err().map(|e| format!("{e}")).unwrap_or_default();
            warn!(
                "{} could not be kept running ({failure}) — this stream falls back to the CPU",
                self.name
            );
            let mut software = Backend::Software(Box::new(super::codec_h264::H264Encoder::new(
                &self.config,
            )?));
            outcome = encode_with(&mut software, pixels, width, height, format);
            *slot = Some((software, width, height));
            self.fresh = true;
            self.budget.reset();
        }

        let (data, is_keyframe) = outcome?;
        drop(slot);

        // Emitting anything means this transform is past its build, so a
        // keyframe request from here on has to rebuild to be honoured.
        if !data.is_empty() {
            self.fresh = false;
            if self.budget.encoded_a_frame() {
                debug!(
                    "{} has encoded {FRAMES_TO_FORGIVE_A_RESTART} frames since its last \
                     restart — the budget is refilled",
                    self.name
                );
            }
        }

        let seq = {
            let mut counter = self.sequence_counter.lock().unwrap();
            *counter = counter.wrapping_add(1);
            *counter
        };

        Ok(super::encoder::EncodedFrame {
            sequence_number: seq,
            width,
            height,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            is_keyframe,
            data: bytes::Bytes::from(data),
        })
    }

    /// Whatever this was configured to produce, which is no longer only H.264.
    ///
    /// The type name predates HEVC support and is now a misnomer — the transform
    /// is selected by subtype and everything below it is codec-agnostic. Kept
    /// because renaming it reaches further than it is worth; this method is the
    /// one place the difference is observable, so this is the place to be honest.
    fn codec(&self) -> super::encoder::VideoCodec {
        self.config.effective_codec()
    }

    fn reconfigure(&mut self, config: &EncodeConfig) -> Result<()> {
        if self.config == *config {
            return Ok(());
        }

        // Bitrate and frame rate are what the adaptive controller changes, often,
        // and both can be applied to a running transform. Everything else —
        // codec, rate-control mode, quality target, keyframe interval — is
        // negotiated at construction, so a change there does need a rebuild.
        let structural = EncodeConfig {
            target_bitrate_kbps: config.target_bitrate_kbps,
            fps: config.fps,
            ..self.config.clone()
        } != *config;

        let previous = std::mem::replace(&mut self.config, config.clone());
        let mut slot = self.inner.lock().unwrap();

        if structural {
            *slot = None;
            return Ok(());
        }

        if let Some((Backend::Hardware(encoder), _, _)) = slot.as_mut() {
            encoder.frame_duration = 10_000_000 / config.fps.max(1) as i64;
            let bps = bitrate_bps(config, encoder.width, encoder.height);
            if let Err(e) = encoder.set_bitrate(bps) {
                // Falling back to a rebuild rather than silently running at the
                // old rate: the controller is relying on this having happened.
                warn!("Could not set the bitrate live ({e}) — rebuilding the encoder");
                *slot = None;
            }
            return Ok(());
        }

        // The software fallback rebuilds; it is cheap by comparison and this
        // path only runs where hardware was unavailable anyway.
        if let Some((Backend::Software(encoder), _, _)) = slot.as_mut() {
            encoder.reconfigure(config);
        } else {
            let _ = previous;
        }
        Ok(())
    }
}

// ── Bitstream cleanup ────────────────────────────────────────

/// NAL unit types dropped from the encoder's output before it goes on the wire.
///
/// **9, the Access Unit Delimiter, is the one that matters.** AMD's MFT emits an
/// AUD in front of every frame; it is legal H.264 and carries no picture data,
/// and OpenH264's decoder — which is what the guest uses — fails the whole
/// access unit when it sees one. The symptom is total and silent: every frame
/// decodes to an error, the guest shows nothing, and the encoder looks perfect
/// from the host's side because the bytes it produced were fine.
///
/// 12 is filler data, which is padding by definition and equally safe to drop.
const DROPPED_NAL_TYPES: [u8; 2] = [9, 12];

/// Whether this codec's output is the Annex-B H.264 stream the cleanup expects.
///
/// # Why this is not "all of them"
///
/// [`strip_unsupported_nals`] reads a NAL type out of the low five bits of the
/// byte after a start code and drops types 9 and 12. That is the H.264 layout
/// and nothing else's. HEVC puts the type in a different field of a two-byte
/// header, so the same mask lands on unrelated bits — by luck it removes only
/// filler, but it is reading numbers that do not mean what it thinks. AV1 is
/// not a start-code format at all: it is a sequence of OBUs, and hunting for
/// `00 00 01` in one finds byte patterns inside the payload.
///
/// So the cleanup is applied where its reasoning holds and nowhere else. The
/// reason it exists — AMD's H.264 MFT emitting an access unit delimiter that
/// OpenH264 refuses — is a fact about H.264 and about one decoder.
fn needs_annex_b_cleanup(codec: super::encoder::VideoCodec) -> bool {
    matches!(codec, super::encoder::VideoCodec::H264)
}

/// Remove NAL units the guest's decoder cannot handle.
///
/// Rewrites the Annex-B stream in place rather than copying: at 60 fps this runs
/// on every frame, and the common case removes six bytes from the front.
fn strip_unsupported_nals(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len());
    for (start, end) in annex_b_units(stream) {
        // The byte after the start code carries the type in its low 5 bits.
        let nal_type = stream[start] & 0x1F;
        if !DROPPED_NAL_TYPES.contains(&nal_type) {
            // Emit a four-byte start code regardless of what the source used;
            // both are legal and one shape is easier to reason about.
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&stream[start..end]);
        }
    }
    out
}

/// Byte ranges of each NAL unit's payload, excluding its start code.
fn annex_b_units(stream: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            // A payload runs to the next start code, minus its leading zeroes.
            let end = match starts.get(n + 1) {
                // Back off the three-byte code, then any extra zero that made it
                // a four-byte one. Leaving them in is harmless for a decoder but
                // makes the output a byte-for-byte moving target for tests.
                Some(&next) => {
                    let mut end = next - 3;
                    while end > start && stream[end - 1] == 0 {
                        end -= 1;
                    }
                    end
                }
                None => stream.len(),
            };
            (end > start).then_some((start, end))
        })
        .collect()
}

/// Bytes in an NV12 frame: full-size luma plus half-size interleaved chroma.
pub(crate) fn nv12_len(width: u16, height: u16) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + w * (h / 2)
}

/// Pack a ratio into the 64-bit attribute Media Foundation expects.
fn set_ratio(
    media_type: &IMFMediaType,
    key: &GUID,
    numerator: u32,
    denominator: u32,
) -> Result<()> {
    let packed = ((numerator as u64) << 32) | denominator as u64;
    // SAFETY: documented attribute write on a type we own.
    unsafe { media_type.SetUINT64(key, packed)? };
    Ok(())
}

fn bitrate_bps(config: &EncodeConfig, width: u16, height: u16) -> u32 {
    match config.target_bitrate_kbps {
        0 => super::codec_h264::default_bitrate_bps(width, height, config.fps),
        kbps => kbps.saturating_mul(1000),
    }
}

/// BGRA to NV12: luma plane, then interleaved U/V at half resolution.
///
/// Same BT.601 coefficients as the I420 converter — the two must agree, or the
/// picture shifts colour depending on which encoder happens to be in use.
pub fn bgra_to_nv12(bgra: &[u8], width: u16, height: u16, out: &mut Vec<u8>) {
    out.resize(nv12_len(width, height), 0);
    bgra_to_nv12_into(bgra, width, height, out);
}

/// How many row-pairs one worker converts before the split stops paying.
///
/// Below roughly this much work per thread the spawn costs more than it saves,
/// so small frames stay on one thread.
pub(crate) const ROWS_PER_WORKER: usize = 64;

/// As [`bgra_to_nv12`], but into a caller-owned buffer — which is what lets the
/// encoder convert straight into the media buffer the transform will read,
/// rather than into a staging `Vec` that then has to be copied.
///
/// Split across threads because at 2560x1600 this is 4.1 million pixels of
/// scalar arithmetic per frame, measured at ~7 ms — nearly half the hardware
/// encoder's entire frame time, and pure waste on a machine with idle cores.
/// The bands are disjoint slices of the output, so no synchronisation is needed
/// beyond the scope's join.
pub fn bgra_to_nv12_into(bgra: &[u8], width: u16, height: u16, out: &mut [u8]) {
    let (w, h) = (width as usize, height as usize);
    debug_assert!(bgra.len() >= w * h * 4);
    debug_assert!(out.len() >= nv12_len(width, height));

    let row_pairs = h / 2;
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(row_pairs.div_ceil(ROWS_PER_WORKER))
        .max(1);
    let band = row_pairs.div_ceil(workers);

    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    // One band is `band` row-pairs: 2*band luma rows and `band` chroma rows.
    let bands = y_plane
        .chunks_mut(band * 2 * w)
        .zip(uv_plane.chunks_mut(band * w))
        .enumerate();

    if workers == 1 {
        for (index, (y, uv)) in bands {
            convert_band(bgra, w, index * band, y, uv);
        }
        return;
    }

    std::thread::scope(|scope| {
        for (index, (y, uv)) in bands {
            scope.spawn(move || convert_band(bgra, w, index * band, y, uv));
        }
    });
}

/// Convert one horizontal band, starting at `first_row_pair` of the source.
fn convert_band(bgra: &[u8], w: usize, first_row_pair: usize, y_plane: &mut [u8], uv_plane: &mut [u8]) {
    for local in 0..y_plane.len() / (2 * w) {
        let y0 = (first_row_pair + local) * 2;
        let top = &bgra[y0 * w * 4..(y0 + 1) * w * 4];
        let bottom = &bgra[(y0 + 1) * w * 4..(y0 + 2) * w * 4];
        let (y_top, y_bottom) = y_plane[local * 2 * w..(local + 1) * 2 * w].split_at_mut(w);
        let uv_row = local * w;

        for col_pair in 0..w / 2 {
            let x0 = col_pair * 8;
            let px = [
                (top[x0] as i32, top[x0 + 1] as i32, top[x0 + 2] as i32),
                (top[x0 + 4] as i32, top[x0 + 5] as i32, top[x0 + 6] as i32),
                (bottom[x0] as i32, bottom[x0 + 1] as i32, bottom[x0 + 2] as i32),
                (bottom[x0 + 4] as i32, bottom[x0 + 5] as i32, bottom[x0 + 6] as i32),
            ];
            let luma =
                |(b, g, r): (i32, i32, i32)| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            y_top[col_pair * 2] = luma(px[0]);
            y_top[col_pair * 2 + 1] = luma(px[1]);
            y_bottom[col_pair * 2] = luma(px[2]);
            y_bottom[col_pair * 2 + 1] = luma(px[3]);

            let (b, g, r) = (
                (px[0].0 + px[1].0 + px[2].0 + px[3].0) / 4,
                (px[0].1 + px[1].1 + px[2].1 + px[3].1) / 4,
                (px[0].2 + px[1].2 + px[2].2 + px[3].2) / 4,
            );
            // NV12 interleaves U then V, unlike I420's separate planes.
            uv_plane[uv_row + col_pair * 2] =
                (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv_plane[uv_row + col_pair * 2 + 1] =
                (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::VideoCodec;

    /// The probe every hardware test starts with, and a time limit to go with it.
    ///
    /// # Why the limit is here and not left to whoever is watching
    ///
    /// Roughly one full-suite run in ten stops dead in one of the tests below,
    /// at no CPU, and never comes back — see [`crate::watchdog`], which records
    /// what was measured and why the cause is not something this repository can
    /// reach. Every test that reaches a graphics driver takes the guard, and
    /// holds it for as long as it holds an encoder, so a stall ends the run in
    /// a minute with a name on it instead of waiting forever in silence.
    ///
    /// Returning both together is what makes that hard to forget: there is no
    /// way to get at the encoder without also getting the limit that bounds it.
    #[must_use]
    fn hardware(test: &'static str) -> Option<(EncoderInfo, crate::watchdog::Guard)> {
        // The limit starts *before* the probe, not after it. The first `probe()`
        // in a process is the call that enumerates transforms and activates one
        // — a driver call like any other here, and one that would otherwise sit
        // outside the bound that exists for it.
        let limit = crate::watchdog::limit(test);
        Some((probe()?, limit))
    }

    fn pattern(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                let left = x < width as usize / 2;
                data[i] = if left { 30 } else { 200 };
                data[i + 1] = 40;
                data[i + 2] = if left { 220 } else { 40 };
                data[i + 3] = 255;
            }
        }
        data
    }

    /// The bug this replaces, stated as a test.
    ///
    /// Three display switches, each followed by a healthy run of frames, used
    /// to exhaust a lifetime count and end the session with "the encoder has
    /// stalled" while the encoder was working perfectly well.
    #[test]
    fn restarts_spaced_by_healthy_frames_never_run_out() {
        let mut budget = RestartBudget::default();
        for _ in 0..10 {
            assert!(budget.has_room(), "a recovered restart must not be held against the next");
            budget.spend();
            for _ in 0..FRAMES_TO_FORGIVE_A_RESTART {
                budget.encoded_a_frame();
            }
        }
        assert!(budget.has_room());
    }

    /// And the case the budget exists for, which still has to end.
    #[test]
    fn restarts_that_never_encode_anything_run_out() {
        let mut budget = RestartBudget::default();
        for _ in 0..MAX_ENCODER_RESTARTS {
            assert!(budget.has_room());
            budget.spend();
        }
        assert!(!budget.has_room(), "a transform that never recovers has to stop being retried");
    }

    /// A run cut short by the next stall is not a recovery.
    #[test]
    fn a_short_run_of_frames_does_not_refill_the_budget() {
        let mut budget = RestartBudget::default();
        budget.spend();
        for _ in 0..FRAMES_TO_FORGIVE_A_RESTART - 1 {
            assert!(!budget.encoded_a_frame());
        }
        assert_eq!(budget.spent, 1);
        assert!(budget.encoded_a_frame(), "the refill is reported exactly once");
        assert_eq!(budget.spent, 0);
        assert!(!budget.encoded_a_frame(), "and not again on the frames after it");
    }

    #[test]
    fn nv12_is_sized_and_laid_out_correctly() {
        assert_eq!(nv12_len(64, 32), 64 * 32 + 64 * 16);

        let mut out = Vec::new();
        bgra_to_nv12(&pattern(64, 32), 64, 32, &mut out);
        assert_eq!(out.len(), nv12_len(64, 32));

        // Left half is red-dominant, right half blue: luma must differ across
        // the split, or the plane is transposed or misaligned.
        let row = &out[..64];
        let avg = |s: &[u8]| s.iter().map(|v| *v as u32).sum::<u32>() / s.len() as u32;
        assert_ne!(avg(&row[..32]), avg(&row[32..]));
    }

    #[test]
    fn nv12_and_i420_agree_on_colour() {
        // Both converters feed H.264 encoders that are selected at runtime, so
        // a mismatch would tint the picture depending on which one ran.
        let (w, h) = (64usize, 32usize);
        let src = pattern(w as u16, h as u16);

        let mut i420 = crate::codec_h264::I420Frame::new(w, h);
        i420.read_bgra(&src);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&src, w as u16, h as u16, &mut nv12);

        use openh264::formats::YUVSource;
        assert_eq!(&nv12[..w * h], i420.y(), "luma planes must be identical");

        // Chroma is interleaved here and planar there; de-interleave to compare.
        let uv = &nv12[w * h..];
        let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
        let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(u.as_slice(), i420.u());
        assert_eq!(v.as_slice(), i420.v());
    }

    #[test]
    fn nv12_conversion_reuses_its_buffer() {
        let mut out = Vec::new();
        bgra_to_nv12(&pattern(64, 32), 64, 32, &mut out);
        let capacity = out.capacity();
        bgra_to_nv12(&pattern(64, 32), 64, 32, &mut out);
        assert_eq!(out.capacity(), capacity, "a per-frame realloc would be wasteful");
    }

    /// Build an Annex-B stream from `(nal_type, payload_length)` pairs.
    fn annex_b(units: &[(u8, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, (nal_type, len)) in units.iter().enumerate() {
            // Alternate three- and four-byte start codes: both are legal and a
            // parser that only handles one is a real failure mode.
            out.extend_from_slice(if index % 2 == 0 { &[0, 0, 0, 1][..] } else { &[0, 0, 1][..] });
            out.push(*nal_type);
            // Non-zero filler, so a payload byte is never mistaken for padding.
            out.extend(std::iter::repeat_n(0xAAu8, *len));
        }
        out
    }

    /// Types of each NAL in an Annex-B stream, in order.
    fn nal_types(stream: &[u8]) -> Vec<u8> {
        annex_b_units(stream).iter().map(|(start, _)| stream[*start] & 0x1F).collect()
    }

    #[test]
    fn access_unit_delimiters_are_removed_and_picture_data_is_not() {
        // The bug this exists for: AMD's encoder puts an AUD in front of every
        // frame, OpenH264 rejects the whole access unit when it sees one, and
        // the failure surfaces as an out-of-memory error naming nothing.
        // 7=SPS, 8=PPS, 5=IDR, 1=slice, 9=AUD, 12=filler, 6=SEI.
        let stream = annex_b(&[(9, 1), (7, 20), (8, 6), (6, 30), (5, 400), (12, 900)]);

        let cleaned = strip_unsupported_nals(&stream);
        assert_eq!(nal_types(&cleaned), vec![7, 8, 6, 5], "wrong units survived");
        // The picture data itself must come through untouched.
        assert!(cleaned.windows(400).any(|w| w.iter().all(|b| *b == 0xAA)));
    }

    #[test]
    fn a_stream_with_nothing_to_strip_keeps_every_unit() {
        let stream = annex_b(&[(7, 20), (8, 6), (5, 100), (1, 50), (1, 50)]);
        assert_eq!(nal_types(&strip_unsupported_nals(&stream)), vec![7, 8, 5, 1, 1]);
    }

    #[test]
    fn stripping_survives_input_that_is_not_a_bitstream() {
        // The encoder's output is trusted, but a truncated or empty sample must
        // not panic the capture thread — that would end the session.
        assert!(strip_unsupported_nals(&[]).is_empty());
        assert!(strip_unsupported_nals(&[0, 0, 1]).is_empty(), "a start code with no payload");
        assert!(strip_unsupported_nals(&[0xAA; 16]).is_empty(), "no start code at all");
        strip_unsupported_nals(&[0, 0, 0, 1, 9]);
    }

    /// Whether this machine has a hardware encoder at all. Reported rather than
    /// asserted — CI and VMs legitimately have none.
    #[test]
    fn report_available_hardware_encoder() {
        let _limit = crate::watchdog::limit("report_available_hardware_encoder");
        match probe() {
            Some(info) => println!("hardware encoder: {}", info.name),
            None => println!("no hardware H.264 encoder on this machine"),
        }
    }

    /// `probe` must stay cheap after the first call.
    ///
    /// Regression guard with real teeth. It answers by *activating* a transform,
    /// which took 188 ms on the machine this was written on, and it reads like a
    /// getter — so the settings panel called it twice a frame and the whole
    /// window redrew at under 3 fps. Anything that reintroduces the work per
    /// call fails here rather than in someone's dropdown.
    #[test]
    fn the_encoder_probe_is_cheap_once_the_answer_is_known() {
        let _limit = crate::watchdog::limit("the_encoder_probe_is_cheap_once_the_answer_is_known");
        // Pay for the first one; it is allowed to be slow.
        let first = probe();

        let started = std::time::Instant::now();
        for _ in 0..50 {
            assert_eq!(std::hint::black_box(probe()), first, "the answer must not drift");
        }
        let each = started.elapsed() / 50;

        // Enormously generous against 188 ms: this is checking that the work is
        // not being redone, not benchmarking a clone.
        assert!(
            each < std::time::Duration::from_millis(1),
            "probe() costs {each:?} per call — it is enumerating every time, \
             which at two calls a frame is a UI that renders at 3 fps"
        );
    }

    /// What this machine can encode with, and whether a preference is honoured.
    ///
    /// Run with `cargo test --release -p tain-core -- --ignored --nocapture
    /// what_encoders`. On a laptop with two GPUs this is what says whether the
    /// picker will have anything to offer.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn what_encoders_this_machine_offers() {
        let names = available_encoders();
        println!("{} hardware H.264 encoders:", names.len());
        for name in &names {
            println!("  {name}");
        }

        // A preference that matches something must win, whatever the order the
        // system enumerated them in.
        if let Some(target) = names.first().cloned() {
            let word = target.split_whitespace().next().unwrap_or(&target).to_string();
            let config = EncodeConfig { preferred_encoder: word.clone(), ..Default::default() };
            if let Ok(encoder) = MediaFoundationEncoder::new(&config, 320, 240) {
                println!("preferring {word:?} selected {}", encoder.info.name);
                assert!(
                    encoder.info.name.to_lowercase().contains(&word.to_lowercase()),
                    "asked for {word:?} and got {}",
                    encoder.info.name
                );
            }
        }
    }

    /// Moving content has to arrive where it was sent.
    ///
    /// # Why the still picture was not enough
    ///
    /// The other round trip sends one picture thirty times. Every inter frame
    /// is then "copy the previous one", which is the easiest possible thing to
    /// predict — it catches a decoder that produces nothing, and nothing else.
    /// It cannot see a picture that is *wrong*, which is what was reported: the
    /// smearing where part of the screen keeps content that has moved on.
    ///
    /// So this moves a bright bar across a dark field, one step per frame, and
    /// asks of every decoded frame whether the bar is where that frame said it
    /// would be. A frame predicted from the wrong reference, or built partly
    /// from an older one, leaves the bar behind — and this notices, because it
    /// checks the place the bar has left as well as the place it should be.
    ///
    /// Lossy compression is not a problem for this: the bar is a quarter of the
    /// width and the test asks only which of two regions is brighter, which no
    /// bitrate this runs at can reverse.
    #[test]
    #[ignore = "requires a hardware encoder and decoder"]
    fn moving_content_arrives_where_it_was_sent() {
        use crate::encoder::VideoCodec;

        let (w, h) = (640u16, 480u16);
        const FRAMES: usize = 40;
        let bar = w as usize / 4;
        // Where the bar starts on frame `i`, always well inside the picture.
        let at = |i: usize| (i * 8) % (w as usize - bar);

        let frame_at = |i: usize| {
            let mut data = vec![0u8; w as usize * h as usize * 4];
            let start = at(i);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let px = (y * w as usize + x) * 4;
                    let bright = x >= start && x < start + bar;
                    let v = if bright { 235 } else { 20 };
                    data[px] = v;
                    data[px + 1] = v;
                    data[px + 2] = v;
                    data[px + 3] = 255;
                }
            }
            data
        };

        let mut faults: Vec<String> = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            if !codec.is_encodable_here() {
                eprintln!("no {codec:?} encoder here — skipping");
                continue;
            }
            let config = EncodeConfig { codec, fps: 30, ..EncodeConfig::default() };
            let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("encoder");
            let Ok(mut decoder) = crate::decode_ffmpeg::FfmpegDecoder::for_codec(codec) else {
                eprintln!("no {codec:?} decoder here — skipping");
                continue;
            };

            // The encoder holds frames back, so what comes out is behind what
            // went in. Counting decoded pictures rather than assuming a
            // one-to-one match is what keeps this honest about which frame it
            // is looking at.
            let mut decoded_index = 0usize;
            let (mut right, mut wrong) = (0u32, 0u32);
            for i in 0..FRAMES {
                let source = frame_at(i);
                let Ok((packet, _)) = encoder.encode(&source, w, h, PixelFormat::Bgra) else {
                    continue;
                };
                if packet.is_empty() {
                    continue;
                }
                let Ok(Some(picture)) = decoder.decode(&packet) else {
                    continue;
                };

                let expected = at(decoded_index);
                decoded_index += 1;

                let row = h as usize / 2;
                let sample = |x: usize| {
                    let px = (row * w as usize + x) * 4;
                    picture.rgba[px] as i32
                };
                // The middle of where the bar should be, and the middle of where
                // it was two steps ago — which is dark unless something is
                // holding on to an older frame.
                let here = sample(expected + bar / 2);
                let behind_at = at(decoded_index.saturating_sub(3));
                let behind = sample(behind_at + bar / 2);

                if here > 140 && (behind_at.abs_diff(expected) < bar || behind < 140) {
                    right += 1;
                } else {
                    if wrong < 3 {
                        eprintln!(
                            "{codec:?} frame {decoded_index}: bar should be at {expected}, \
                             brightness there {here}, and {behind} where it was three frames ago"
                        );
                    }
                    wrong += 1;
                }
            }

            if right + wrong < 10 {
                faults.push(format!("{codec:?}: only {} pictures came back", right + wrong));
            } else if wrong > 0 {
                faults.push(format!(
                    "{codec:?}: {wrong} of {} frames had the bar in the wrong place or left a \
                     copy of it behind — content from an earlier frame surviving into a later \
                     one, which is what smearing is",
                    right + wrong
                ));
            } else {
                println!("{codec:?}: {right} moving frames, all where they were sent");
            }
        }

        assert!(faults.is_empty(), "{}", faults.join("\n"));
    }

    /// A picture must survive the round trip *looking like itself*.
    ///
    /// # What the existing round-trip tests do not check
    ///
    /// They assert that a picture came out, which is a real bar — a stream that
    /// encodes and never decodes hid behind the host's own preview for a week.
    /// But a picture that comes out mangled passes it, and a guest reported
    /// exactly that: HEVC arriving as a green field with red text, structure
    /// intact and colour destroyed.
    ///
    /// They also feed BGRA, which is no longer what a Windows host sends. The
    /// capture backend now converts on the GPU and hands the encoder NV12
    /// directly, so the path in production is the one path nothing covered.
    ///
    /// Two colours, in large flat blocks so that lossy compression cannot
    /// account for a difference, and the assertion is on which channel wins
    /// rather than on a value — chroma read from the wrong place does not shift
    /// a colour, it replaces it.
    ///
    /// # This currently fails, and is committed failing on purpose
    ///
    /// H.264 passes through both input formats: mean R=129 G=40 B=115, which is
    /// the picture that went in. H.265 comes back R=6 G=124 B=0 on every frame
    /// — flat green, no chroma — through the same encoder wrapper, the same
    /// converter and the same decoder.
    ///
    /// Ruled out so far, by measurement rather than reasoning: the input format
    /// (it fails from BGRA as well as NV12, so it is not the direct-NV12
    /// capture); NAL stripping (the only unit dropped from the HEVC stream is
    /// type 38, filler, which is correct); output-sample concatenation (one
    /// sample per frame, distinct timestamps); and the decode surface being
    /// padded to 640x512 for a 640x480 picture, which is real and now described
    /// correctly, and changed nothing.
    ///
    /// Left failing because a green picture is what a user sees and this says
    /// so in one line. It is `#[ignore]`d, so it costs no one a red suite.
    #[test]
    #[ignore = "requires a hardware encoder and decoder; HEVC currently fails it"]
    fn a_picture_keeps_its_colours_through_every_codec_and_input_format() {
        use crate::encoder::VideoCodec;

        let (w, h) = (640u16, 480u16);
        let bgra = pattern(w, h);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        // Collected rather than asserted one at a time. Stopping at the first
        // bad codec hides every codec after it, and "which of these work" is
        // the question this is here to answer.
        let mut faults: Vec<String> = Vec::new();

        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            if !codec.is_encodable_here() {
                eprintln!("no {codec:?} encoder here — skipping");
                continue;
            }
            for (label, source, format) in
                [("BGRA", &bgra, PixelFormat::Bgra), ("NV12", &nv12, PixelFormat::Nv12)]
            {
                let config = EncodeConfig { codec, fps: 30, ..EncodeConfig::default() };
                let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("encoder");
                let Ok(mut decoder) = crate::decode_ffmpeg::FfmpegDecoder::for_codec(codec) else {
                    eprintln!("no {codec:?} decoder here — skipping");
                    continue;
                };

                // The source is red-dominant on the left and blue-dominant on
                // the right, sampled well inside each block and away from the
                // seam. Reported as "is red winning where red should win",
                // because chroma taken from the wrong place does not shift a
                // colour, it replaces it.
                let reads_right = |rgba: &[u8]| {
                    let at = |x: usize, y: usize| {
                        let i = (y * w as usize + x) * 4;
                        (rgba[i] as i32, rgba[i + 2] as i32)
                    };
                    let (left_r, left_b) = at(w as usize / 4, h as usize / 2);
                    let (right_r, right_b) = at(w as usize * 3 / 4, h as usize / 2);
                    left_r > left_b + 40 && right_b > right_r + 40
                };

                // Counted apart, because they fail apart: see the note above.
                let (mut key_ok, mut key_bad) = (0u32, 0u32);
                let (mut inter_ok, mut inter_bad) = (0u32, 0u32);
                for _ in 0..30 {
                    let Ok((packet, keyframe)) = encoder.encode(source, w, h, format) else {
                        continue;
                    };
                    if packet.is_empty() {
                        continue;
                    }
                    if let Ok(Some(picture)) = decoder.decode(&packet) {
                        assert_eq!(picture.rgba.len(), w as usize * h as usize * 4);
                        match (keyframe, reads_right(&picture.rgba)) {
                            (true, true) => key_ok += 1,
                            (true, false) => key_bad += 1,
                            (false, true) => inter_ok += 1,
                            (false, false) => inter_bad += 1,
                        }
                    }
                }

                let total = key_ok + inter_ok + key_bad + inter_bad;
                if total <= 5 {
                    faults.push(format!("{codec:?}/{label}: produced almost no pictures ({total})"));
                    continue;
                }
                if key_bad > 0 {
                    faults.push(format!(
                        "{codec:?}/{label}: {key_bad} keyframe(s) came back the wrong colour"
                    ));
                }
                if inter_bad > 0 {
                    faults.push(format!(
                        "{codec:?}/{label}: keyframes fine ({key_ok}), {inter_bad} inter frames \
                         a flat neutral grey against {inter_ok} right — a decoder writing \
                         nothing into the surface it hands back, which on real content is a \
                         picture that starts blank and fills in only where something moved"
                    ));
                }
                if faults.is_empty() {
                    println!("{codec:?}/{label}: {key_ok} keyframes and {inter_ok} inter frames, \
                              all the right colours");
                }
            }
        }

        assert!(faults.is_empty(), "{}", faults.join("\n"));
    }

    /// HEVC end to end: encoded on this GPU, decoded on this GPU.
    ///
    /// The bar is a *picture out of the decoder*, not bytes out of the encoder —
    /// the same bar H.264 is held to, and for the same reason: a stream that
    /// encodes and does not decode looked fine from the host for a week once
    /// already.
    #[test]
    #[ignore = "requires a hardware H.265 encoder and decoder"]
    fn h265_survives_this_machines_encoder_and_decoder() {
        use crate::encoder::VideoCodec;

        if !VideoCodec::H265.is_encodable_here() {
            eprintln!("no HEVC encoder on this machine — skipping");
            return;
        }
        let (w, h) = (640u16, 480u16);
        let config =
            EncodeConfig { codec: VideoCodec::H265, fps: 30, ..EncodeConfig::default() };
        assert_eq!(config.effective_codec(), VideoCodec::H265, "must not have fallen back");

        let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("HEVC encoder");
        println!("encoding HEVC with {}", encoder.info.name);

        let Ok(mut decoder) = crate::decode_ffmpeg::FfmpegDecoder::for_codec(VideoCodec::H265)
        else {
            eprintln!("no D3D11VA HEVC decoder on this machine — skipping");
            return;
        };

        let mut decoded = 0;
        let mut produced = 0;
        for frame in 0..30 {
            let mut src = vec![0u8; w as usize * h as usize * 4];
            for (i, px) in src.chunks_exact_mut(4).enumerate() {
                px[0] = (i / 5 + frame * 7) as u8;
                px[1] = (i / 3 + frame * 3) as u8;
                px[2] = (i + frame * 11) as u8;
                px[3] = 255;
            }
            let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else { continue };
            if packet.is_empty() {
                continue;
            }
            produced += 1;
            if decoder.decode(&packet).expect("HEVC must decode").is_some() {
                decoded += 1;
            }
        }

        println!("{produced} HEVC packets, {decoded} decoded to a picture");
        assert!(produced > 5, "the encoder produced almost nothing");
        assert!(decoded > 0, "{produced} HEVC packets and none of them decoded");
    }

    /// A GOP of zero has to mean "never", not "use your own default".
    ///
    /// # Why this needs measuring rather than reading
    ///
    /// The whole point of asking for zero is to remove the periodic bandwidth
    /// and decode spike a keyframe causes — nothing here can lose a frame in
    /// transit, so periodic ones buy nothing. But `CODECAPI_AVEncMPVGOPSize` is
    /// a vendor-implemented knob, and "0" is exactly the kind of value a driver
    /// might read as "unset, pick something sensible". If it does, the spike is
    /// still there and the fix is imaginary.
    ///
    /// So this counts them. One keyframe — the first — is correct. More than one
    /// in three hundred frames means this encoder inserts its own and the
    /// interval cannot be relied on for it.
    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn a_zero_keyframe_interval_really_stops_them() {
        let Some(info) = probe() else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let (w, h) = (640u16, 480u16);
        let config = EncodeConfig { keyframe_interval: 0, fps: 60, ..EncodeConfig::default() };
        let mut encoder = MediaFoundationEncoder::new(&config, w, h).expect("encoder");

        // Moving content, so the encoder has a reason to want a fresh keyframe
        // and is not simply repeating a static picture.
        let mut keyframes = 0;
        let mut produced = 0;
        for frame in 0..300 {
            let mut src = vec![0u8; w as usize * h as usize * 4];
            for (i, px) in src.chunks_exact_mut(4).enumerate() {
                px[0] = (i / 5 + frame * 7) as u8;
                px[1] = (i / 3 + frame * 3) as u8;
                px[2] = (i + frame * 11) as u8;
                px[3] = 255;
            }
            let Ok((packet, is_keyframe)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else {
                continue;
            };
            if packet.is_empty() {
                continue;
            }
            produced += 1;
            if is_keyframe {
                keyframes += 1;
            }
        }

        println!("{}: {keyframes} keyframes in {produced} frames", info.name);
        assert!(produced > 100, "only {produced} frames came out — the test proved nothing");
        assert_eq!(
            keyframes, 1,
            "{} inserted its own keyframes despite a GOP of 0 — the periodic spike is still there",
            info.name
        );
    }

    /// Which profiles the *fallback* decoder can actually read.
    ///
    /// # What this measured, and the wrong answer it gave first
    ///
    /// The profile was hard-coded to Baseline because OpenH264 — the decoder a
    /// guest without the ffmpeg runtime falls back to — was said to handle
    /// nothing else. That is worth testing, because Baseline is also the profile
    /// Intel Quick Sync stalls on, so the constraint costs a whole class of host.
    ///
    /// The first version of this test asserted that *some* frame decoded, and
    /// Main and High each cleared that bar. They should not have: they decoded
    /// six packets out of twenty and failed on the other fourteen, and the
    /// failures were being discarded by an `.ok()`. A stream that decodes three
    /// frames in ten is not a working stream — it is the original bug report,
    /// reproduced and then waved through.
    ///
    /// So it counts failures now, and demands that a profile decode cleanly or
    /// not be the default. See [`H264Profile`].
    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn openh264_decodes_every_profile_this_can_produce() {
        use crate::codec_h264::H264Decoder;
        use crate::encoder::H264Profile;

        let Some(_) = probe() else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let (w, h) = (320u16, 240u16);
        let mut outcomes: Vec<(H264Profile, u32, u32)> = Vec::new();

        for profile in H264Profile::ALL {
            let config = EncodeConfig { profile, fps: 30, ..EncodeConfig::default() };
            let Ok(mut encoder) = MediaFoundationEncoder::new(&config, w, h) else {
                eprintln!("{} refused by this encoder — skipping", profile.label());
                continue;
            };
            let mut decoder = H264Decoder::new().expect("OpenH264");

            // Moving content: a still picture can be carried by a keyframe
            // alone, which would not exercise the profile's inter prediction.
            let mut decoded = 0;
            let mut refused = 0;
            for frame in 0..20 {
                let mut src = vec![0u8; w as usize * h as usize * 4];
                for (i, px) in src.chunks_exact_mut(4).enumerate() {
                    px[0] = (i / 3 + frame * 5) as u8;
                    px[1] = (i / 7 + frame * 3) as u8;
                    px[2] = (i + frame * 11) as u8;
                    px[3] = 255;
                }
                let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else {
                    continue;
                };
                if packet.is_empty() {
                    continue;
                }
                match decoder.decode(&packet) {
                    Ok(Some(_)) => decoded += 1,
                    // Not the same as an error: the decoder legitimately holds
                    // a packet back until it has enough to emit a picture.
                    Ok(None) => {}
                    Err(_) => refused += 1,
                }
            }
            println!("{}: {decoded} decoded, {refused} refused", profile.label());
            outcomes.push((profile, decoded, refused));
        }

        // The default has to be a profile the fallback decoder reads *cleanly*.
        // Anything else is a guest watching a stream that mostly does not
        // arrive, which is worse than a profile that compresses less well.
        let default = outcomes
            .iter()
            .find(|(profile, _, _)| *profile == H264Profile::default())
            .expect("the default profile was not exercised");
        assert_eq!(
            default.2,
            0,
            "the default profile ({}) was refused {} times by OpenH264 — a guest \
             decoding on the CPU would see a broken stream",
            default.0.label(),
            default.2
        );
        assert!(default.1 > 0, "the default profile decoded nothing at all");
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn a_frame_round_trips_through_the_hardware_encoder() {
        let Some(info) = probe() else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        println!("encoding with {}", info.name);

        let config = EncodeConfig { codec: VideoCodec::H264, fps: 60, ..EncodeConfig::default() };
        let mut enc = MediaFoundationEncoder::new(&config, 640, 480).expect("encoder");
        let mut dec = crate::codec_h264::H264Decoder::new().unwrap();

        // The bar is a *picture out of the decoder*, not merely bytes out of the
        // encoder. An earlier version of this test only checked that something
        // was produced, and every frame of every real session failed to decode
        // for a week without a single test noticing.
        // Two different pictures, alternated, so the encoder has motion to
        // describe rather than twenty identical frames it can skip entirely.
        let src = pattern(640, 480);
        let moved: Vec<u8> = src.iter().map(|b| b.wrapping_add(60)).collect();

        let mut produced = 0;
        let mut decoded = 0;
        for i in 0..20 {
            let (packet, _) = enc
                .encode(if i % 2 == 0 { &src } else { &moved }, 640, 480, PixelFormat::Bgra)
                .expect("encode");
            if packet.is_empty() {
                continue;
            }
            produced += 1;
            if dec.decode(&packet).expect("the hardware bitstream must be valid H.264").is_some() {
                decoded += 1;
            }
        }

        assert!(produced > 0, "the encoder produced nothing across 20 frames");
        assert!(
            decoded > 0,
            "{produced} packets came out of the encoder and none of them decoded"
        );
    }

    #[test]
    fn a_size_the_gpu_cannot_take_degrades_to_the_cpu_instead_of_failing() {
        // 16x16 is below every hardware encoder's minimum. On Auto that has to
        // keep streaming — the same path covers a GPU that has run out of
        // concurrent encode sessions, which is what a fourth guest hits.
        use crate::encoder::VideoEncoder;
        let Some((_, limit)) = hardware("a_size_the_gpu_cannot_take_degrades_to_the_cpu")
        else {
            return;
        };

        let config = EncodeConfig::default();
        limit.at("building the wrapper — enumerate + ActivateObject");
        let mut enc = HardwareH264Encoder::new(&config, true).expect("wrapper");

        limit.at("encoding 16x16, which the GPU refuses and the CPU takes over");
        let frame = enc.encode_frame(&pattern(16, 16), 16, 16, PixelFormat::Bgra);
        assert!(frame.is_ok(), "Auto must not drop a session over frame size: {frame:?}");
        assert!(!enc.is_using_hardware(), "16x16 cannot have been encoded on the GPU");

        // And the same encoder goes back to hardware at a size it can take.
        limit.at("encoding 640x480, which rebuilds the hardware encoder");
        enc.encode_frame(&pattern(640, 480), 640, 480, PixelFormat::Bgra).expect("encode");
        assert!(enc.is_using_hardware());

        limit.at("dropping the encoder — ShutdownObject");
    }

    #[test]
    fn asking_for_hardware_explicitly_refuses_rather_than_downgrading() {
        use crate::encoder::VideoEncoder;
        let Some((_, _limit)) = hardware("asking_for_hardware_explicitly_refuses") else {
            return;
        };

        let config = EncodeConfig::default();
        let mut enc = HardwareH264Encoder::new(&config, false).expect("wrapper");
        assert!(
            enc.encode_frame(&pattern(16, 16), 16, 16, PixelFormat::Bgra).is_err(),
            "a silent CPU downgrade would make the Hardware setting meaningless"
        );
    }

    /// The format may change between frames, and the encoder must honour it.
    ///
    /// Two places now depend on this. `DesktopFollowing` negotiates NV12 while
    /// knowing a consent prompt will interrupt it with BGRA frames from the
    /// secure-desktop agent, and `DxgiCapturer` drops back to BGRA mid-session
    /// when a rebuilt duplication cannot carry its GPU converter. Both are only
    /// safe because a frame says what it is and this reads that field per call.
    ///
    /// There are two ways to break that, and only one of them is loud. An
    /// encoder that *rejected* a format change would fail the first half here —
    /// that is the regression this codebase has already had once, in
    /// `encode_with`. An encoder that *ignored* the field would raise no error
    /// at all: BGRA read as NV12 is the right number of bytes for a picture a
    /// quarter the size, so it encodes happily and arrives as noise.
    ///
    /// So the second half measures rather than trusts, and calibrates itself on
    /// whatever card is running it: the same picture down both honest paths
    /// should cost about the same, and misreading it should cost enormously
    /// more. On an AMD VCE the three totals are 1112, 1112 and 255762 bytes —
    /// the honest pair agree exactly, because the encoder's own conversion is
    /// the one this test used to build the NV12.
    #[test]
    fn a_frame_may_change_format_mid_stream() {
        use crate::encoder::VideoEncoder;
        let Some((_, limit)) = hardware("a_frame_may_change_format_mid_stream") else {
            return;
        };

        let (w, h) = (640u16, 480u16);
        let bgra = pattern(w, h);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        let config = EncodeConfig::default();

        // ── a stream that changes format is not an error ──
        limit.at("building the first encoder");
        let mut enc = HardwareH264Encoder::new(&config, true).expect("wrapper");
        limit.at("encoding the mixed-format stream");
        // NV12 first, so anything cached from the opening frame is the format
        // that later has to give way.
        enc.encode_frame(&nv12, w, h, PixelFormat::Nv12).expect("NV12 opening frame");
        enc.encode_frame(&bgra, w, h, PixelFormat::Bgra)
            .expect("a BGRA frame after NV12 — this is the consent prompt");
        enc.encode_frame(&nv12, w, h, PixelFormat::Nv12)
            .expect("and NV12 again once the prompt has gone");

        // ── and the format is read, not assumed ──
        limit.at("dropping the first encoder and building three more");
        let mut honest_nv12 = HardwareH264Encoder::new(&config, true).expect("wrapper");
        let mut honest_bgra = HardwareH264Encoder::new(&config, true).expect("wrapper");
        let mut misread = HardwareH264Encoder::new(&config, true).expect("wrapper");
        limit.at("encoding twelve frames through each of the three");
        let (mut nv12_bytes, mut bgra_bytes, mut misread_bytes) = (0usize, 0usize, 0usize);
        // Enough frames to drain the transform: a hardware encoder holds the
        // first several and returns nothing for them, so one frame measures
        // zero against zero and would pass whatever the code did.
        for _ in 0..12 {
            let of = |f: crate::encoder::EncodedFrame| f.data.len();
            nv12_bytes += of(honest_nv12.encode_frame(&nv12, w, h, PixelFormat::Nv12).unwrap());
            bgra_bytes += of(honest_bgra.encode_frame(&bgra, w, h, PixelFormat::Bgra).unwrap());
            misread_bytes += of(misread.encode_frame(&bgra, w, h, PixelFormat::Nv12).unwrap());
        }

        assert!(
            bgra_bytes * 10 < misread_bytes,
            "the same BGRA frames cost {bgra_bytes} bytes labelled BGRA and \
             {misread_bytes} labelled NV12 — too close for the label to be doing \
             anything, so the format field is being ignored and a mixed stream \
             would arrive as noise"
        );
        assert!(
            bgra_bytes <= nv12_bytes * 2 && nv12_bytes <= bgra_bytes * 2,
            "one picture cost {bgra_bytes} bytes as BGRA and {nv12_bytes} as NV12; \
             the two paths are meant to reach the same place"
        );
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn changing_the_bitrate_does_not_stall_the_stream() {
        // The adaptive controller changes bitrate whenever the link moves, and
        // rebuilding the transform for it cost hundreds of milliseconds on the
        // capture thread. That stall was charged to whichever frame it
        // interrupted, which reported as "slow encoding", which made the
        // controller adapt again — a loop that fed on its own cost.
        use crate::encoder::VideoEncoder;
        let Some(_) = probe() else { return };

        let config = EncodeConfig { fps: 60, target_bitrate_kbps: 20_000, ..EncodeConfig::default() };
        let mut enc = HardwareH264Encoder::new(&config, false).expect("encoder");
        let frame = pattern(640, 480);
        for _ in 0..5 {
            enc.encode_frame(&frame, 640, 480, PixelFormat::Bgra).expect("encode");
        }

        enc.reconfigure(&EncodeConfig { target_bitrate_kbps: 4_000, ..config.clone() })
            .expect("reconfigure");
        assert!(enc.is_using_hardware(), "a bitrate change must not drop the transform");

        let started = std::time::Instant::now();
        enc.encode_frame(&frame, 640, 480, PixelFormat::Bgra).expect("encode");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(80),
            "the frame after a bitrate change took {elapsed:?} — that is a rebuild"
        );

        // A structural change still rebuilds, because it has to.
        enc.reconfigure(&EncodeConfig {
            keyframe_interval: 30,
            target_bitrate_kbps: 4_000,
            ..config
        })
        .expect("reconfigure");
        enc.encode_frame(&frame, 640, 480, PixelFormat::Bgra).expect("encode");
        assert!(enc.is_using_hardware());
    }

    /// Which frame sizes this machine's hardware encoder will actually take.
    ///
    /// # Why this exists
    ///
    /// An AMD AV1 transform stalled — stopped asking for input, for longer than
    /// [`ENCODER_STALL_TIMEOUT`] — a couple of seconds into every session that
    /// ran at a size whose axes were not multiples of sixteen, and streamed
    /// perfectly at 1920x1200. Every log said "the encoder has stalled" and
    /// nothing said what it disagreed with, so the sizes are asked one at a
    /// time and the answers written down.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn what_frame_sizes_this_encoder_takes() {
        use crate::encoder::{VideoCodec, VideoEncoder};

        // Enough frames to get past the first one or two, which an async
        // transform legitimately swallows before producing anything.
        const FRAMES: usize = 12;

        // Pairs that differ only in alignment, plus the real sizes from the
        // reported sessions: 1920x1200 streamed, 1398x874 stalled.
        const SIZES: [(u16, u16); 9] = [
            (1920, 1200),
            (1398, 874),
            (1392, 864),
            (1398, 864),
            (1392, 874),
            (2880, 1620),
            (2880, 1616),
            (1636, 1022),
            (1632, 1024),
        ];

        // One encoder per size, built and dropped before the next — never two
        // at once, because holding a second hardware transform on this hardware
        // hung the run outright. `TAIN_SIZES=WxH,WxH` narrows it.
        let sizes: Vec<(u16, u16)> = match std::env::var("TAIN_SIZES") {
            Ok(list) => list
                .split(',')
                .filter_map(|s| s.split_once('x'))
                .filter_map(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                .collect(),
            Err(_) => SIZES.to_vec(),
        };
        let codec = match std::env::var("TAIN_CODEC").as_deref() {
            Ok("h264") => VideoCodec::H264,
            Ok("h265") => VideoCodec::H265,
            _ => VideoCodec::Av1,
        };

        let config = EncodeConfig { codec, ..EncodeConfig::default() };
        println!("\n{} — a fresh encoder per size", codec.label());
        for (w, h) in sizes {
            let Ok(mut enc) = HardwareH264Encoder::new(&config, false) else {
                println!("  no hardware {} encoder", codec.label());
                return;
            };
            let frame = pattern(w, h);
            let started = std::time::Instant::now();
            let mut outcome = Ok(());
            let mut packets = 0;
            for _ in 0..FRAMES {
                match enc.encode_frame(&frame, w, h, PixelFormat::Bgra) {
                    Ok(f) if f.data.is_empty() => {}
                    Ok(_) => packets += 1,
                    Err(e) => {
                        outcome = Err(e);
                        break;
                    }
                }
            }
            let took = started.elapsed();
            // Against the rule this measured: width on the superblock grid,
            // height on sixteen. A size marked `off` and encoding fine, or one
            // marked `on` and stalling, means the rule has moved.
            let align = |w: u16, h: u16| {
                if w.is_multiple_of(64) && h.is_multiple_of(16) { "on" } else { "off" }
            };
            match outcome {
                Ok(()) => println!(
                    "  {w}x{h} (grid {}) — {packets}/{FRAMES} packets in {took:?}",
                    align(w, h),
                ),
                Err(e) => println!(
                    "  {w}x{h} (grid {}) — FAILED after {packets} packets in {took:?}: {e}",
                    align(w, h),
                ),
            }
            // Dropped here, and given a moment: the transform hands its encode
            // session back on drop, and the driver does not appear to be
            // instant about it.
            drop(enc);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn the_encoder_rejects_a_size_it_was_not_built_for() {
        let Some(_) = probe() else { return };
        let config = EncodeConfig::default();
        let mut enc = MediaFoundationEncoder::new(&config, 640, 480).unwrap();
        let err = enc.encode(&pattern(320, 240), 320, 240, PixelFormat::Bgra).unwrap_err();
        assert!(err.to_string().contains("built for"));
    }
}
