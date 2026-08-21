//! The host's desktop wallpaper, as a preview-sized picture.
//!
//! ## Why the wallpaper rather than the screen
//!
//! A card in the connections list answers one question: *which machine is
//! this?* A screenshot answers it badly. It is whatever happened to be on
//! screen at the moment of connecting — a lock screen, a browser, someone's
//! inbox — so the same machine looks different every time, and the picture kept
//! on another person's disk is a picture of their work.
//!
//! The wallpaper is better on both counts. It is the thing people actually
//! recognise a machine by, it does not change between sessions, and it is
//! already a decorative image its owner chose to look at. Nothing private
//! leaves the host.
//!
//! Falls back to a frame of the screen when there is no wallpaper to read: a
//! solid-colour desktop, a policy hiding the path, an unreadable file. See the
//! caller in [`super::network`] for that path.

use anyhow::Result;

/// Read the current wallpaper and reduce it to a preview.
///
/// `None` rather than an error for "there is no wallpaper", which is an
/// ordinary state and not worth a warning on every connection.
pub fn preview() -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    match windows_impl::wallpaper_preview() {
        Ok(picture) => picture,
        Err(e) => {
            // Debug, not warn: falling back to a frame of the screen is a fine
            // outcome, and this fires on every machine with no wallpaper set.
            tracing::debug!("Could not read the wallpaper ({e:#}) — using a frame instead");
            None
        }
    }

    #[cfg(not(target_os = "windows"))]
    None
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::Result;
    use anyhow::Context;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    use crate::preview::{from_frame, H, W};
    use crate::screen_capture::PixelFormat;

    /// Longest path `SPI_GETDESKWALLPAPER` writes, including the terminator.
    const MAX_PATH: usize = 260;

    /// Undo `CoInitializeEx` only when this call is what initialised the thread.
    ///
    /// Media Foundation initialises COM on the threads it runs on, and a blind
    /// `CoUninitialize` here would decrement someone else's count and pull the
    /// apartment out from under the encoder.
    struct ComGuard(bool);

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: paired with the CoInitializeEx that returned success.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn init_com() -> ComGuard {
        // SAFETY: documented initialisation. A thread already in an apartment
        // returns a failure we deliberately tolerate rather than treat as one.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard(hr.is_ok())
    }

    /// Path of the current wallpaper, or `None` when none is set.
    fn wallpaper_path() -> Option<Vec<u16>> {
        let mut buffer = [0u16; MAX_PATH];
        // SAFETY: the buffer is MAX_PATH wide, the documented maximum this
        // call writes.
        unsafe {
            SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buffer.len() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
        }

        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        // A solid-colour desktop reports an empty path rather than failing,
        // which is the common case this has to handle quietly.
        if end == 0 {
            return None;
        }
        let mut path = buffer[..end].to_vec();
        path.push(0);
        Some(path)
    }

    pub fn wallpaper_preview() -> Result<Option<Vec<u8>>> {
        let Some(path) = wallpaper_path() else { return Ok(None) };
        let _com = init_com();

        // SAFETY: a documented COM sequence, on objects created here and
        // released when they go out of scope at the end of this function.
        unsafe {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .context("no WIC imaging factory")?;

            let decoder = factory
                .CreateDecoderFromFilename(
                    PCWSTR(path.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
                .context("the wallpaper file could not be decoded")?;
            let frame = decoder.GetFrame(0).context("the wallpaper has no first frame")?;

            // Converted before scaling: the scaler wants a known layout, and a
            // wallpaper can be a palettised PNG or a CMYK JPEG as easily as
            // plain RGB.
            let converter = factory.CreateFormatConverter()?;
            converter
                .Initialize(
                    &frame,
                    &GUID_WICPixelFormat32bppBGRA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .context("the wallpaper is in a format WIC cannot convert")?;

            // Fant is WIC's area-averaging filter, which is the right one for a
            // reduction this large — a bilinear sample would alias a detailed
            // wallpaper into noise rather than averaging it down.
            //
            // Aspect ratio is deliberately not preserved. A 16:10 wallpaper
            // squeezed into 16:9 is imperceptible once blurred, and letterboxing
            // would put black bars in a card meant to read as a colour.
            let scaler = factory.CreateBitmapScaler()?;
            scaler
                .Initialize(&converter, W as u32, H as u32, WICBitmapInterpolationModeFant)
                .context("the wallpaper could not be scaled")?;

            let stride = W * 4;
            let mut bgra = vec![0u8; stride * H];
            scaler
                .CopyPixels(std::ptr::null(), stride as u32, &mut bgra)
                .context("the scaled wallpaper could not be read")?;

            // Straight through the ordinary path, so the wallpaper gets the
            // same blur as a screen frame and the two are indistinguishable
            // downstream.
            Ok(from_frame(&bgra, W, H, PixelFormat::Bgra))
        }
    }
}
