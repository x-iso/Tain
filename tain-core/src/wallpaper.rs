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
//! On Windows this falls back to a frame of the screen when there is no
//! wallpaper to read: a solid-colour desktop, a policy hiding the path, an
//! unreadable file. See the caller in [`super::network`] for that path.
//!
//! On Linux it never does. There is no one place a wallpaper is kept, so "could
//! not read it" is far more likely here, and would turn into exactly the picture
//! of somebody's work this exists to avoid. [`linux_impl`] answers with a plain
//! colour instead.

#[cfg(target_os = "windows")]
use anyhow::Result;

/// Read the current wallpaper and reduce it to a preview.
///
/// `None` rather than an error for "there is no wallpaper", which is an
/// ordinary state and not worth a warning on every connection.
pub fn preview() -> Option<Vec<u8>> {
    #[cfg(target_os = "linux")]
    return Some(linux_impl::preview());

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

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    None
}

/// Where each desktop keeps its wallpaper, and reading it.
///
/// # Why this asks every desktop in turn
///
/// The wallpaper portal only *sets* one, and nothing else is common to them, so
/// each desktop is asked in its own way: `gsettings` for GNOME and its
/// descendants, Plasma's config file, `xfconf-query`, and the command line of a
/// wallpaper program for the compositors that leave it to one. The desktop the
/// session says it is goes first, because a machine with two desktops installed
/// keeps stale settings for the one that is not running.
#[cfg(target_os = "linux")]
mod linux_impl {
    use crate::preview::{from_rgba, H, W};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// What a desktop with no readable wallpaper shows on its card: a colour
    /// close to the app's own background, rather than a picture of the screen.
    const PLAIN: [u8; 3] = [0x2b, 0x2f, 0x36];

    /// How long reading the wallpaper may hold up the session it is for. The
    /// answer comes from other programs, and from a disk, and a card is never
    /// worth waiting on.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(3);

    pub fn preview() -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read());
        });
        match rx.recv_timeout(PATIENCE) {
            Ok(Some(picture)) => picture,
            Ok(None) => plain(),
            Err(_) => {
                tracing::debug!("The wallpaper took too long to read — showing a plain card");
                plain()
            }
        }
    }

    fn plain() -> Vec<u8> {
        PLAIN.iter().copied().cycle().take(crate::preview::BYTES).collect()
    }

    fn read() -> Option<Vec<u8>> {
        let found = find();
        tracing::debug!("The wallpaper is {found:?}");
        decode(&found?)
    }

    fn decode(path: &Path) -> Option<Vec<u8>> {
        let image = match image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
            Ok(reader) => reader.decode(),
            Err(e) => {
                tracing::debug!("Could not open the wallpaper {}: {e}", path.display());
                return None;
            }
        };
        let image = match image {
            Ok(image) => image,
            Err(e) => {
                tracing::debug!("Could not decode the wallpaper {}: {e}", path.display());
                return None;
            }
        };
        // Squeezed rather than cropped, for the reason on the Windows path: a
        // 16:10 wallpaper in 16:9 is imperceptible once blurred.
        let small = image.resize_exact(W as u32, H as u32, image::imageops::FilterType::Triangle).to_rgba8();
        from_rgba(small.as_raw(), W, H)
    }

    fn find() -> Option<PathBuf> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
        let is = |names: &[&str]| names.iter().any(|name| desktop.contains(name));

        let first: Option<fn() -> Option<PathBuf>> = if is(&["kde", "plasma"]) {
            Some(plasma)
        } else if is(&["cinnamon"]) {
            Some(cinnamon)
        } else if is(&["mate"]) {
            Some(mate)
        } else if is(&["xfce"]) {
            Some(xfce)
        } else if is(&["gnome", "unity", "ubuntu", "budgie", "pantheon", "cosmic"]) {
            Some(gnome)
        } else {
            None
        };
        first
            .and_then(|ask| ask())
            .or_else(wallpaper_program)
            .or_else(|| first.is_none().then(gnome).flatten())
            .or_else(|| first.is_none().then(plasma).flatten())
            .or_else(|| first.is_none().then(xfce).flatten())
    }

    fn output(program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn gsetting(schema: &str, key: &str) -> Option<String> {
        unquoted(&output("gsettings", &["get", schema, key])?)
    }

    fn gnome() -> Option<PathBuf> {
        let schema = "org.gnome.desktop.background";
        // GNOME keeps a wallpaper per colour scheme and shows the one in force.
        let dark = gsetting("org.gnome.desktop.interface", "color-scheme").as_deref() == Some("prefer-dark");
        dark.then(|| gsetting(schema, "picture-uri-dark"))
            .flatten()
            .and_then(|value| usable(&value))
            .or_else(|| usable(&gsetting(schema, "picture-uri")?))
    }

    fn cinnamon() -> Option<PathBuf> {
        usable(&gsetting("org.cinnamon.desktop.background", "picture-uri")?)
    }

    fn mate() -> Option<PathBuf> {
        usable(&gsetting("org.mate.background", "picture-filename")?)
    }

    fn xfce() -> Option<PathBuf> {
        let properties = output("xfconf-query", &["-c", "xfce4-desktop", "-l"])?;
        properties
            .lines()
            .filter(|property| property.ends_with("/last-image"))
            .find_map(|property| usable(&output("xfconf-query", &["-c", "xfce4-desktop", "-p", property])?))
    }

    fn config_home() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }

    fn plasma() -> Option<PathBuf> {
        const FILE: &str = "plasma-org.kde.plasma.desktop-appletsrc";
        let chosen = [config_home().map(|c| c.join(FILE)), Some(PathBuf::from("/etc/xdg").join(FILE))]
            .into_iter()
            .flatten()
            .filter_map(|file| std::fs::read_to_string(file).ok())
            .find_map(|text| plasma_image(&text));
        if let Some(found) = chosen.and_then(|value| usable(&value)) {
            return Some(found);
        }
        // Never changed from the default, so it is in no config file: the look
        // and feel in use names it. Distributions ship their own this way.
        let package = config_home()
            .and_then(|config| std::fs::read_to_string(config.join("kdeglobals")).ok())
            .and_then(|text| ini_value(&text, "[KDE]", "LookAndFeelPackage"))
            .unwrap_or_else(|| "org.kde.breeze.desktop".to_string());
        let defaults = format!("/usr/share/plasma/look-and-feel/{package}/contents/defaults");
        let name = std::fs::read_to_string(defaults)
            .ok()
            .and_then(|text| ini_value(&text, "[Wallpaper]", "Image"))
            .unwrap_or_else(|| "Next".to_string());
        usable(&format!("/usr/share/wallpapers/{name}"))
    }

    /// The program that draws the wallpaper on a compositor that has no settings
    /// of its own, from how it was started.
    fn wallpaper_program() -> Option<PathBuf> {
        for process in std::fs::read_dir("/proc").ok()?.flatten() {
            let Ok(raw) = std::fs::read(process.path().join("cmdline")) else { continue };
            let args: Vec<String> = raw
                .split(|byte| *byte == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect();
            if let Some(found) = swaybg_image(&args).and_then(|value| usable(&value)) {
                return Some(found);
            }
        }
        let hyprpaper = std::fs::read_to_string(config_home()?.join("hypr/hyprpaper.conf")).ok()?;
        hyprpaper_image(&hyprpaper).and_then(|value| usable(&value))
    }

    /// A setting's value as a file that exists and is a picture.
    fn usable(value: &str) -> Option<PathBuf> {
        let path = to_path(value)?;
        if path.is_dir() {
            return package_image(&path);
        }
        if !path.is_file() {
            return None;
        }
        // GNOME's timed wallpapers are an XML list of pictures. Any of them says
        // which machine this is, so the first one does.
        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("xml")) {
            return usable(&xml_first_picture(&std::fs::read_to_string(path).ok()?)?);
        }
        Some(path)
    }

    /// `file:///a%20b.jpg`, `/a b.jpg` or `~/a b.jpg` as a path.
    fn to_path(value: &str) -> Option<PathBuf> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if let Some(rest) = value.strip_prefix("file://") {
            return Some(PathBuf::from(percent_decoded(rest)));
        }
        if let Some(rest) = value.strip_prefix("~/") {
            return Some(PathBuf::from(std::env::var_os("HOME")?).join(rest));
        }
        Some(PathBuf::from(value))
    }

    /// The largest picture in a Plasma wallpaper package, which is a folder of
    /// the same picture at many sizes named for them, such as `1920x1080.png`.
    fn package_image(package: &Path) -> Option<PathBuf> {
        let files = std::fs::read_dir(package.join("contents/images"))
            .or_else(|_| std::fs::read_dir(package))
            .ok()?;
        files
            .flatten()
            .map(|file| file.path())
            .filter(|path| {
                path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    ["png", "jpg", "jpeg", "webp"].contains(&e.to_lowercase().as_str())
                })
            })
            .max_by_key(|path| {
                let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
                pixels_in_name(name)
            })
    }

    fn pixels_in_name(name: &str) -> u64 {
        let Some((w, h)) = name.split_once('x') else { return 0 };
        match (w.parse::<u64>(), h.parse::<u64>()) {
            (Ok(w), Ok(h)) => w * h,
            _ => 0,
        }
    }

    /// `'value'`, as `gsettings get` prints a string, without its quotes.
    fn unquoted(printed: &str) -> Option<String> {
        let printed = printed.trim();
        let inner = printed.strip_prefix('\'')?.strip_suffix('\'')?;
        Some(inner.replace("\\'", "'"))
    }

    fn percent_decoded(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let hex = (bytes[at] == b'%')
                .then(|| text.get(at + 1..at + 3))
                .flatten()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok());
            match hex {
                Some(byte) => {
                    out.push(byte);
                    at += 3;
                }
                None => {
                    out.push(bytes[at]);
                    at += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The `Image=` of the first desktop in Plasma's applets file. Each screen
    /// and activity has a section of its own, and any of them says which
    /// machine this is.
    fn plasma_image(text: &str) -> Option<String> {
        let mut inside = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                inside = line.ends_with("[Wallpaper][org.kde.image][General]");
            } else if inside
                && let Some(value) = line.strip_prefix("Image=")
                && !value.trim().is_empty()
            {
                return Some(value.trim().to_string());
            }
        }
        None
    }

    fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
        let mut inside = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                inside = line == section;
            } else if inside
                && let Some((name, value)) = line.split_once('=')
                && name.trim() == key
            {
                return Some(value.trim().to_string());
            }
        }
        None
    }

    /// The first picture named in a GNOME timed wallpaper.
    fn xml_first_picture(text: &str) -> Option<String> {
        ["<file>", "<from>"]
            .iter()
            .filter_map(|tag| {
                let at = text.find(tag)?;
                let rest = &text[at + tag.len()..];
                Some((at, rest[..rest.find('<')?].trim().to_string()))
            })
            .min_by_key(|(at, _)| *at)
            .map(|(_, picture)| picture)
    }

    fn swaybg_image(args: &[String]) -> Option<String> {
        let program = args.first()?;
        if Path::new(program).file_name()?.to_str()? != "swaybg" {
            return None;
        }
        args.iter()
            .position(|arg| arg == "-i" || arg == "--image")
            .and_then(|at| args.get(at + 1))
            .cloned()
    }

    /// `wallpaper = monitor,/path` or `preload = /path` in hyprpaper's config.
    fn hyprpaper_image(text: &str) -> Option<String> {
        let setting = |key: &str| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == key).then(|| value.trim().to_string())
            })
        };
        setting("wallpaper")
            .map(|value| value.rsplit(',').next().unwrap_or_default().trim().to_string())
            .or_else(|| setting("preload"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_gsettings_string_comes_out_without_its_quotes() {
            assert_eq!(unquoted("'file:///usr/share/a.jpg'\n").as_deref(), Some("file:///usr/share/a.jpg"));
            assert_eq!(unquoted("'it\\'s.png'").as_deref(), Some("it's.png"));
            assert_eq!(unquoted("@as []"), None);
        }

        #[test]
        fn a_uri_becomes_the_path_it_names() {
            assert_eq!(to_path("file:///home/me/My%20Pictures/a.jpg"), Some("/home/me/My Pictures/a.jpg".into()));
            assert_eq!(to_path("/usr/share/a.jpg"), Some("/usr/share/a.jpg".into()));
            assert_eq!(to_path("  "), None);
        }

        #[test]
        fn a_stray_percent_is_kept_rather_than_eating_what_follows() {
            assert_eq!(percent_decoded("100%.png"), "100%.png");
            assert_eq!(percent_decoded("a%zz"), "a%zz");
            assert_eq!(percent_decoded("%D1%84"), "ф");
        }

        #[test]
        fn plasmas_wallpaper_is_read_from_the_desktops_own_section() {
            let text = "\
[Containments][1][Applets][5][Configuration][General]
Image=file:///not/this.png

[Containments][2][Wallpaper][org.kde.image][General]
Image=file:///home/me/this.png
SlidePaths=/usr/share/wallpapers/
";
            assert_eq!(plasma_image(text).as_deref(), Some("file:///home/me/this.png"));
            assert_eq!(plasma_image("[Containments][2][Wallpaper][org.kde.image][General]\nImage=\n"), None);
        }

        #[test]
        fn the_largest_size_in_a_package_is_the_one_to_read() {
            assert!(pixels_in_name("3840x2160") > pixels_in_name("1920x1080"));
            assert_eq!(pixels_in_name("screenshot"), 0);
        }

        #[test]
        fn a_timed_wallpaper_gives_its_first_picture() {
            let text = "<background><static><duration>1</duration>
                <file>/usr/share/first.jpg</file></static>
                <transition><from>/usr/share/first.jpg</from><to>/usr/share/second.jpg</to></transition>";
            assert_eq!(xml_first_picture(text).as_deref(), Some("/usr/share/first.jpg"));
        }

        #[test]
        fn swaybg_and_hyprpaper_say_where_theirs_is() {
            let args = ["/usr/bin/swaybg", "-m", "fill", "-i", "/home/me/a.png"].map(String::from);
            assert_eq!(swaybg_image(&args).as_deref(), Some("/home/me/a.png"));
            assert_eq!(swaybg_image(&["firefox".to_string(), "-i".to_string(), "x".to_string()]), None);
            assert_eq!(hyprpaper_image("preload = /a.png\nwallpaper = DP-1,/a.png\n").as_deref(), Some("/a.png"));
        }

        #[test]
        fn a_wallpaper_on_disk_comes_out_at_preview_size() {
            let path = std::env::temp_dir().join(format!("tain-wallpaper-{}.png", std::process::id()));
            image::RgbaImage::from_pixel(320, 200, image::Rgba([200, 30, 30, 255])).save(&path).unwrap();
            let picture = decode(&path);
            let _ = std::fs::remove_file(&path);
            let picture = picture.expect("a PNG reads");
            assert_eq!(picture.len(), crate::preview::BYTES);
            assert!(picture[0] > 150 && picture[1] < 80, "the wallpaper's own colour, not a plain card");
        }

        #[test]
        fn a_desktop_with_nothing_readable_gets_a_plain_card_of_the_right_size() {
            assert_eq!(plain().len(), crate::preview::BYTES);
        }
    }
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
