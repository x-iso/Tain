//! Is the media library here, and if not, put it here — Linux.
//!
//! The Linux counterpart of the Windows `ffmpeg_runtime`, and compiled as that
//! module on Linux so every caller asks the same `ffmpeg_runtime::require()`.
//!
//! ## What is loaded, and why at run time
//!
//! `libtain-media.so` holds the two shims (`db_vaapi.c`, `db_audio.c`) and a
//! static FFmpeg; `tain-core/build.rs` links it and compresses it into the
//! binary. Opening it here, rather than linking it, is what keeps it optional:
//! a library the loader cannot find stops a program before `main`, which is
//! the failure the Windows build delay-loads its DLLs to avoid.
//!
//! ## libva first, and globally
//!
//! FFmpeg's VA-API code calls libva, and the library is linked with those
//! symbols undefined. So libva and libva-drm are opened here first with
//! `RTLD_GLOBAL`, which puts their symbols where the media library's lazy
//! bindings will find them the first time a VA-API function runs. Without
//! libva the media library still opens, because nothing has called those
//! functions yet — and nothing will, because [`vaapi`] says no. That is how a
//! machine with no VA-API driver still gets sound.
//!
//! ## Where it comes from
//!
//! A `libtain-media.so` beside the executable wins, so anyone can substitute
//! their own build of it, which FFmpeg's LGPL asks to be possible. Otherwise
//! the embedded copy is unpacked into an anonymous in-memory file and opened
//! from there, so nothing is written to disk; where the kernel refuses that,
//! into the user's cache directory instead.

use anyhow::{Context, Result};
use std::ffi::{c_void, CStr, CString};
use std::path::PathBuf;
use tracing::*;

/// The compressed library, or empty in a build without one.
static EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libtain-media.so.zst"));

/// Identifies this exact library, for the cache-directory fallback.
const RUNTIME_ID: &str = env!("DB_MEDIA_ID");

/// The `db_media_abi()` this code was written against.
const ABI: i32 = 1;

/// Every function any module resolves. Checked at load, so a library missing
/// one is refused whole rather than failing at the first call.
const SYMBOLS: &[&str] = &[
    "db_media_abi",
    "db_va_decoder_new",
    "db_va_decoder_name",
    "db_va_decoder_send",
    "db_va_decoder_receive",
    "db_va_decoder_free",
    "db_va_encoder_new",
    "db_va_encoder_name",
    "db_va_encoder_encode",
    "db_va_encoder_free",
    "db_va_supports",
    "db_opus_encoder_new",
    "db_opus_frame_size",
    "db_opus_encode",
    "db_opus_encoder_free",
    "db_opus_decoder_new",
    "db_opus_decode",
    "db_opus_decoder_free",
];

struct Runtime {
    handle: *mut c_void,
    vaapi: bool,
}

// SAFETY: the handle is a dlopen handle, which may be used from any thread, and
// it is never closed.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

static RUNTIME: std::sync::OnceLock<Option<Runtime>> = std::sync::OnceLock::new();

fn runtime() -> Option<&'static Runtime> {
    RUNTIME
        .get_or_init(|| {
            let vaapi = open_libva();
            match load() {
                Ok((handle, origin)) => {
                    info!(
                        "Media library loaded from {origin}{}",
                        if vaapi { ", with VA-API" } else { " — no libva, so no GPU video" }
                    );
                    Some(Runtime { handle, vaapi })
                }
                Err(e) => {
                    info!("No media library ({e:#}) — no GPU video and no sound");
                    None
                }
            }
        })
        .as_ref()
}

/// Whether the media library can be called into at all.
pub fn available() -> bool {
    runtime().is_some()
}

/// [`available`] as an error, for constructors to return.
pub fn require() -> Result<()> {
    anyhow::ensure!(
        available(),
        "this build has no media library, or it could not be loaded — see the log"
    );
    Ok(())
}

/// Whether VA-API calls may be made: the library is loaded and libva is here.
///
/// Says nothing about the GPU or its driver; a decoder or encoder finds those
/// out when it opens a device.
pub fn vaapi() -> bool {
    runtime().is_some_and(|r| r.vaapi)
}

/// The address of one of [`SYMBOLS`].
///
/// # Panics
///
/// If the library is not loaded or the name is not in [`SYMBOLS`]: both are
/// programming errors, because every caller checks [`require`] first and every
/// name is checked at load.
pub fn symbol(name: &str) -> usize {
    let runtime = runtime().expect("the media library is used only after require()");
    resolve(runtime.handle, name.trim_end_matches('\0'))
        .unwrap_or_else(|| panic!("{name} was checked at load and is now missing"))
}

/// Declare functions from the media library, resolved on first call.
///
/// The Linux counterpart of an `unsafe extern "C"` block: the same signatures,
/// each turned into a function that looks its address up once and calls
/// through it. Used beside the Windows `extern` blocks rather than instead of
/// them, so the Windows declarations stay exactly as they are.
macro_rules! media_functions {
    ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        $(
            #[allow(clippy::too_many_arguments, dead_code)]
            unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
                let address = *ADDRESS
                    .get_or_init(|| $crate::ffmpeg_runtime::symbol(stringify!($name)));
                // SAFETY: the address is the named function, checked present at
                // load, and the library was built from the C file whose
                // prototype this declaration copies.
                let function: unsafe extern "C" fn($($ty),*) $(-> $ret)? =
                    unsafe { std::mem::transmute::<usize, _>(address) };
                unsafe { function($($arg),*) }
            }
        )*
    };
}
pub(crate) use media_functions;

/// Open libva and libva-drm globally, so the media library's lazy bindings
/// resolve to them. False if either is missing.
fn open_libva() -> bool {
    for name in ["libva.so.2", "libva-drm.so.2"] {
        let c_name = CString::new(name).expect("no NUL in a constant");
        // SAFETY: dlopen with a valid C string; the handle is kept for the life
        // of the process by never closing it.
        let handle = unsafe { libc::dlopen(c_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            info!("{name} is not installed ({}) — VA-API is unavailable", dl_error());
            return false;
        }
    }
    true
}

fn load() -> Result<(*mut c_void, String)> {
    let (handle, origin) = match beside_executable() {
        Some(path) => (open(&path)?, path.display().to_string()),
        None => {
            anyhow::ensure!(!EMBEDDED.is_empty(), "this build carries no media library");
            let bytes = zstd::decode_all(EMBEDDED).context("the embedded library is corrupt")?;
            match open_from_memory(&bytes) {
                Ok(handle) => (handle, "the copy inside this binary".to_string()),
                Err(e) => {
                    debug!("Could not open the media library from memory ({e:#}); using a file");
                    let path = unpack_to_cache(&bytes)?;
                    (open(&path)?, path.display().to_string())
                }
            }
        }
    };

    let missing: Vec<&str> =
        SYMBOLS.iter().copied().filter(|name| resolve(handle, name).is_none()).collect();
    anyhow::ensure!(
        missing.is_empty(),
        "{origin} lacks {} — it is not a libtain-media.so this build can use",
        missing.join(", ")
    );
    let abi = resolve(handle, "db_media_abi").expect("checked above");
    // SAFETY: checked present; the prototype is `int db_media_abi(void)`.
    let abi: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute::<usize, _>(abi) };
    let found = unsafe { abi() };
    anyhow::ensure!(found == ABI, "{origin} is ABI {found}, and this build needs {ABI}");
    Ok((handle, origin))
}

/// A library put beside the executable on purpose.
fn beside_executable() -> Option<PathBuf> {
    let candidate = std::env::current_exe().ok()?.parent()?.join("libtain-media.so");
    candidate.is_file().then_some(candidate)
}

fn open(path: &std::path::Path) -> Result<*mut c_void> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .context("the library path has a NUL in it")?;
    // SAFETY: dlopen with a valid C string. Lazy, so VA-API symbols left
    // undefined resolve only if called; local, so FFmpeg's symbols do not leak
    // into anything else loaded later.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    anyhow::ensure!(!handle.is_null(), "{}", dl_error());
    Ok(handle)
}

/// Open the library from an anonymous memory file, writing nothing to disk.
fn open_from_memory(bytes: &[u8]) -> Result<*mut c_void> {
    use std::io::Write;
    use std::os::fd::FromRawFd;

    // SAFETY: memfd_create with a valid name; the descriptor is owned by the
    // File below and closed when it drops, after dlopen has mapped it.
    let fd = unsafe { libc::memfd_create(c"tain-media".as_ptr(), libc::MFD_CLOEXEC) };
    anyhow::ensure!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).context("could not fill the memory file")?;
    open(&PathBuf::from(format!("/proc/self/fd/{fd}")))
}

/// Unpack into `$XDG_CACHE_HOME/Tain/media-<id>/`, once per build.
fn unpack_to_cache(bytes: &[u8]) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("no cache directory to unpack the media library into")?;
    let dir = base.join("Tain").join(format!("media-{RUNTIME_ID}"));
    let path = dir.join("libtain-media.so");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    // Written beside and renamed, so a second instance starting at the same
    // moment never opens half a file.
    let partial = dir.join(format!("libtain-media.so.{}", std::process::id()));
    std::fs::write(&partial, bytes).with_context(|| format!("could not write {}", partial.display()))?;
    std::fs::rename(&partial, &path).with_context(|| format!("could not move {}", path.display()))?;
    Ok(path)
}

fn resolve(handle: *mut c_void, name: &str) -> Option<usize> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: a live handle and a valid C string.
    let address = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
    (!address.is_null()).then_some(address as usize)
}

fn dl_error() -> String {
    // SAFETY: dlerror returns a thread-local string or null.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "unknown error".to_string();
    }
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The library this build embeds loads, has every symbol, and says the
    /// ABI this code expects. Skipped, loudly, in a build without one.
    #[test]
    fn the_embedded_library_loads() {
        if EMBEDDED.is_empty() {
            eprintln!("this build has no media library — not exercised");
            return;
        }
        assert!(available(), "the embedded media library must load; see the log");
    }

    #[test]
    fn the_symbol_list_has_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for name in SYMBOLS {
            assert!(seen.insert(name), "{name} is listed twice");
        }
    }
}
