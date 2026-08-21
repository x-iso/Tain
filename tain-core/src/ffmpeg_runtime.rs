//! Is ffmpeg here, and if not, put it here.
//!
//! ## Why every entry point has to ask
//!
//! The ffmpeg DLLs are delay loaded so that a build without them falls back to
//! the software decoder instead of refusing to start — see `tain-cli/build.rs`.
//! Delay loading moves the failure from "before `main`" to "the first call",
//! and a delay-load failure raises a **structured exception**, which unwinds
//! straight past Rust's error handling and takes the process with it.
//!
//! So nothing may call into the shim without checking first. The first version
//! of this checked in [`super::decode_ffmpeg`] only, and the Opus encoder and
//! decoder went unguarded — which turned a startup failure into a *mid-session
//! crash* on a machine where the exe had been copied without its DLLs. That is
//! strictly worse than what delay loading replaced.
//!
//! Every constructor that reaches C goes through [`available`].
//!
//! ## One file to copy
//!
//! The three DLLs are compressed into the executable at build time and written
//! out on first run. Copying `tain.exe` to another machine is enough —
//! which is what someone actually does, and what they did, and it produced a
//! Windows error box with no explanation.
//!
//! Delay loading is what makes this possible at all: nothing touches an ffmpeg
//! symbol until [`available`] has run, so there is a moment in which to put the
//! libraries somewhere the loader will find them.
//!
//! DLLs sitting beside the executable win over the embedded copy, so a
//! development build uses the tree it was built against and anyone wanting to
//! substitute their own ffmpeg — which the LGPL requires be possible — can.

#![cfg(all(target_os = "windows", db_ffmpeg))]

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Compressed at build time by `embed_runtime`, in dependency order.
///
/// Loading them in this order matters: each is loaded by absolute path, and a
/// dependency loaded first is already in the module list when the next one
/// needs it, so nothing has to be searched for.
const EMBEDDED: [(&str, &[u8]); 3] = [
    ("avutil-59.dll", include_bytes!(concat!(env!("OUT_DIR"), "/avutil-59.dll.zst"))),
    ("swresample-5.dll", include_bytes!(concat!(env!("OUT_DIR"), "/swresample-5.dll.zst"))),
    ("avcodec-61.dll", include_bytes!(concat!(env!("OUT_DIR"), "/avcodec-61.dll.zst"))),
];

/// Identifies this exact set of libraries; see `embed_runtime`.
const RUNTIME_ID: &str = env!("DB_RUNTIME_ID");

/// Worked out once. Extraction is not free, and this is asked on every decoder
/// and every codec construction.
static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether the ffmpeg runtime can be called into at all.
pub fn available() -> bool {
    *PRESENT.get_or_init(|| {
        // Beside the executable wins: a development build should use the tree it
        // was built against, and replacing the shipped libraries has to stay
        // possible.
        if load_by_name() {
            return true;
        }
        match unpack_and_load() {
            Ok(dir) => {
                tracing::debug!("Using the embedded ffmpeg runtime from {}", dir.display());
                true
            }
            Err(e) => {
                tracing::info!(
                    "No ffmpeg runtime ({e:#}) — hardware decode and audio are unavailable"
                );
                false
            }
        }
    })
}

/// Fail before touching a delay-loaded import that cannot be resolved.
pub fn require() -> Result<()> {
    anyhow::ensure!(
        available(),
        "the ffmpeg runtime could not be loaded — hardware decode and audio are unavailable"
    );
    Ok(())
}

/// Try the ordinary search order, which finds DLLs beside the executable.
fn load_by_name() -> bool {
    for (name, _) in EMBEDDED {
        if load(Path::new(name)).is_none() {
            return false;
        }
    }
    true
}

/// Write the embedded libraries out and load them from there.
fn unpack_and_load() -> Result<PathBuf> {
    anyhow::ensure!(
        EMBEDDED.iter().all(|(_, bytes)| !bytes.is_empty()),
        "this build has no embedded ffmpeg runtime"
    );

    // Keyed on the build, so an updated ffmpeg lands in its own directory rather
    // than being confused with what is already cached.
    let dir = cache_dir()?.join(RUNTIME_ID);
    std::fs::create_dir_all(&dir)?;

    for (name, packed) in EMBEDDED {
        let path = dir.join(name);
        // Written once. Re-extracting 66 MB on every launch would be a visible
        // pause for nothing, and the directory name already says which build
        // these belong to.
        if !path.exists() {
            let bytes = zstd::decode_all(packed)
                .map_err(|e| anyhow::anyhow!("could not decompress {name}: {e}"))?;
            // Through a temporary file and a rename, so a second instance
            // starting at the same moment never sees a half-written DLL.
            let temp = dir.join(format!("{name}.{}.part", std::process::id()));
            std::fs::write(&temp, &bytes)?;
            // Losing the race is fine: whoever won wrote identical bytes.
            if std::fs::rename(&temp, &path).is_err() {
                let _ = std::fs::remove_file(&temp);
            }
        }
        load(&path).ok_or_else(|| anyhow::anyhow!("could not load {}", path.display()))?;
    }
    Ok(dir)
}

/// Where extracted libraries live between runs.
fn cache_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("no writable location for the ffmpeg runtime"))?;
    Ok(base.join("Tain").join("runtime"))
}

/// Load one library, keeping it for the life of the process.
fn load(path: &Path) -> Option<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;

    // SAFETY: a library load by path. The handle is deliberately never freed —
    // the process keeps it for as long as anything might call into it.
    let loaded = unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())) };
    match loaded {
        Ok(handle) if !handle.is_invalid() => Some(()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the answer, asking twice must give the same one.
    ///
    /// The check is cached, and a cache that could disagree with itself would
    /// let one call site take the hardware path while another took the fallback.
    #[test]
    fn the_answer_is_stable() {
        let first = available();
        assert_eq!(first, available());
        assert_eq!(first, require().is_ok());
    }

    /// The build must actually have embedded something.
    ///
    /// An empty blob is how `embed_runtime` reports "no ffmpeg tree to embed",
    /// and it would otherwise show up as a portable build that silently is not.
    #[test]
    fn the_runtime_is_embedded_and_compressed() {
        for (name, packed) in EMBEDDED {
            assert!(!packed.is_empty(), "{name} was not embedded");
            // zstd frames start with a fixed magic number; anything else means
            // raw bytes were embedded by mistake and the exe is 66 MB heavier
            // than it needs to be.
            assert_eq!(
                &packed[..4],
                &[0x28, 0xb5, 0x2f, 0xfd],
                "{name} is not a zstd frame"
            );
        }
        assert_eq!(RUNTIME_ID.len(), 16, "the runtime id should be a 64-bit hex fingerprint");
    }

    /// Decompression has to produce a real PE image, not just bytes.
    #[test]
    fn each_embedded_library_decompresses_to_a_dll() {
        for (name, packed) in EMBEDDED {
            let bytes = zstd::decode_all(packed).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(bytes.len() > 100_000, "{name} decompressed to {} bytes", bytes.len());
            // "MZ" — every Windows executable image starts with it.
            assert_eq!(&bytes[..2], b"MZ", "{name} did not decompress to a PE image");
        }
    }
}
