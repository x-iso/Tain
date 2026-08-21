//! Compiles the libavcodec shim, when there is an ffmpeg tree to compile against.
//!
//! ffmpeg is **optional**. Without it the crate still builds and still decodes —
//! see `codec_mf_decode` and `decode_d3d11va` — so this script never fails the
//! build over a missing dependency. It reports what it found and sets the
//! `db_ffmpeg` cfg only when the hardware path can actually be compiled.

use std::path::{Path, PathBuf};

fn main() {
    // # Reconstructed, and worth checking
    //
    // These three lines were the whole of the previous build script's output,
    // recovered from `target/debug/build/tain-core-*/output` after this file was
    // overwritten by mistake. The directives are exact; any conditional logic
    // around them that did not run on this machine is not.
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    println!("cargo:rerun-if-env-changed=NASM");
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rerun-if-changed=ffmpeg/db_decode.c");
    println!("cargo:rerun-if-changed=ffmpeg/db_audio.c");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    // Declared so `cfg(db_ffmpeg)` in the crate is checked rather than assumed
    // to be a typo.
    println!("cargo:rustc-check-cfg=cfg(db_ffmpeg)");

    if !cfg!(target_os = "windows") {
        return;
    }

    let Some(dir) = ffmpeg_dir() else {
        // Named rather than merely described: the fix is one command, and a
        // warning that says what is missing without saying how to get it just
        // moves the search somewhere else.
        //
        // Deliberately not fetched here. ffmpeg is optional — everything below
        // is skipped and the crate still builds and still decodes — so a
        // `cargo build` that quietly downloaded 149 MB would be a surprise,
        // would fail offline, and would repeat on every clean CI runner.
        println!(
            "cargo:warning=no ffmpeg tree found — building without hardware decode. \
             Run scripts/fetch-ffmpeg.ps1 to download it, or set FFMPEG_DIR \n             to point at an existing shared build."
        );
        return;
    };

    let include = dir.join("include");
    let lib = dir.join("lib");
    if !include.join("libavcodec/avcodec.h").exists() {
        println!(
            "cargo:warning=ffmpeg at {} has no include/libavcodec/avcodec.h — \
             building without hardware decode",
            dir.display()
        );
        return;
    }

    cc::Build::new()
        .file("ffmpeg/db_decode.c")
        .file("ffmpeg/db_audio.c")
        .include(&include)
        // ffmpeg's headers use C99 designated initialisers and mixed
        // declarations; MSVC needs telling which dialect that is.
        .flag_if_supported("/std:c11")
        // The COM interfaces in hwcontext_d3d11va.h are the C vtable form, and
        // only when the header is not being read as C++.
        .define("COBJMACROS", None)
        .warnings(false)
        .compile("db_decode");

    // # Regenerated import libraries, not the shipped ones
    //
    // The prebuilt ffmpeg is a MinGW build and its `.lib` files are GNU-format
    // import libraries — symbols like `__libavcodec_avcodec_lib_iname` rather
    // than MSVC's `__IMPORT_DESCRIPTOR_*`. link.exe resolves against those
    // happily, but `/DELAYLOAD` only builds thunks for MSVC-style descriptors,
    // so delay loading silently does nothing: no error, no LNK4199, and an
    // empty Delay Import Directory in the result.
    //
    // That matters because ffmpeg is supposed to be optional. Load-time linked,
    // a missing DLL kills the process before `main` with a system error box and
    // no log line, instead of falling back to the software decoder.
    //
    // Rebuilding the import libraries from the DLLs' own export tables produces
    // MSVC-format ones. The shipped directory is deliberately NOT added to the
    // search path when that succeeds — with both present the GNU ones win and
    // the imports stay load-time.
    match msvc_import_libs(&dir) {
        Some(generated) => {
            println!("cargo:rustc-link-search=native={}", generated.display());
        }
        None => {
            println!("cargo:rustc-link-search=native={}", lib.display());
            println!(
                "cargo:warning=using ffmpeg's shipped import libraries — a missing DLL will                  fail before main() instead of falling back to the software decoder"
            );
        }
    }
    // The runtime is compressed into the binary so a single exe is portable;
    // see `ffmpeg_runtime` for how it comes back out.
    embed_runtime(&dir);

    println!("cargo:rustc-link-lib=avcodec");
    println!("cargo:rustc-link-lib=avutil");
    println!("cargo:rustc-cfg=db_ffmpeg");
}

/// Where to look for a prebuilt ffmpeg, in order of how explicit it is.
fn ffmpeg_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("FFMPEG_DIR") {
        let path = PathBuf::from(dir);
        return path.exists().then_some(path);
    }
    // The default location, relative to the workspace rather than this crate,
    // so both crates in the workspace resolve it the same way.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let candidate = Path::new(&manifest).parent()?.join("third_party/ffmpeg");
    candidate.exists().then_some(candidate)
}

/// Rebuild MSVC-format import libraries from the DLLs' export tables.
///
/// Returns the directory holding them, or `None` if the toolchain pieces are
/// missing — in which case the caller falls back to the shipped GNU ones.
///
/// The generated libraries land in `OUT_DIR` and are searched *before* the
/// shipped ones, so the same `-l avcodec` picks these up.
fn msvc_import_libs(dir: &Path) -> Option<PathBuf> {
    let target = std::env::var("TARGET").ok()?;
    // `find` hands back a ready-to-run Command with the toolchain environment
    // already set, which is the whole reason for using it over a PATH lookup.
    let dumpbin_for = || cc::windows_registry::find(&target, "dumpbin.exe");
    let lib_for = || cc::windows_registry::find(&target, "lib.exe");
    dumpbin_for()?;
    lib_for()?;
    let out = PathBuf::from(std::env::var("OUT_DIR").ok()?).join("implib");
    std::fs::create_dir_all(&out).ok()?;

    for (dll, name) in [("avcodec-61.dll", "avcodec"), ("avutil-59.dll", "avutil")] {
        let dll_path = dir.join("bin").join(dll);
        if !dll_path.exists() {
            return None;
        }
        let lib_path = out.join(format!("{name}.lib"));
        // Regenerating on every build would run two processes over a 66 MB DLL
        // for nothing; the export table only changes when the DLL does.
        if is_newer(&lib_path, &dll_path) {
            continue;
        }

        let exports = dumpbin_for()?
            .args(["/NOLOGO", "/EXPORTS"])
            .arg(&dll_path)
            .output()
            .ok()?;
        if !exports.status.success() {
            return None;
        }
        let names = parse_exports(&String::from_utf8_lossy(&exports.stdout));
        if names.is_empty() {
            return None;
        }

        let def_path = out.join(format!("{name}.def"));
        let mut def = format!("LIBRARY {dll}\nEXPORTS\n");
        for symbol in &names {
            def.push_str(symbol);
            def.push('\n');
        }
        std::fs::write(&def_path, def).ok()?;

        let made = lib_for()?
            .arg("/NOLOGO")
            .arg("/MACHINE:X64")
            .arg(format!("/DEF:{}", def_path.display()))
            .arg(format!("/OUT:{}", lib_path.display()))
            .output()
            .ok()?;
        if !made.status.success() {
            return None;
        }
    }
    Some(out)
}

/// Whether `a` exists and is at least as new as `b`.
fn is_newer(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    match (a.modified(), b.modified()) {
        (Ok(a), Ok(b)) => a >= b,
        _ => false,
    }
}

/// Exported symbol names from `dumpbin /EXPORTS` output.
///
/// The table rows are `ordinal hint RVA name`; everything before the header and
/// after the summary is prose. Rows are recognised by shape rather than by
/// locating the header, which differs between toolchain versions.
fn parse_exports(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 4 {
            continue;
        }
        // ordinal, hint (hex), RVA (hex), name.
        if fields[0].parse::<u32>().is_err() {
            continue;
        }
        if u32::from_str_radix(fields[1], 16).is_err() {
            continue;
        }
        if u32::from_str_radix(fields[2], 16).is_err() {
            continue;
        }
        let name = fields[3];
        if name.starts_with('[') || name.is_empty() {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

/// The DLLs that have to exist at runtime, in dependency order.
///
/// Exactly three. avcodec pulls in avutil and swresample and nothing else —
/// avdevice, avfilter, avformat and swscale were being shipped for years of
/// nobody calling them, at 53 MB.
pub const RUNTIME_DLLS: [&str; 3] = ["avutil-59.dll", "swresample-5.dll", "avcodec-61.dll"];

/// Compress each runtime DLL into `OUT_DIR` for the crate to embed.
///
/// # Why compressed
///
/// avcodec alone is 63 MB, and an executable that large is unpleasant to copy
/// around — which is the entire problem being solved. zstd takes the three down
/// to roughly a third of that, and the cost is paid once at build time and a
/// few milliseconds on first run.
fn embed_runtime(dir: &Path) {
    let Ok(out) = std::env::var("OUT_DIR") else { return };
    let out = PathBuf::from(out);
    let mut fingerprint: u64 = 0xcbf2_9ce4_8422_2325;

    for dll in RUNTIME_DLLS {
        let from = dir.join("bin").join(dll);
        let to = out.join(format!("{dll}.zst"));
        println!("cargo:rerun-if-changed={}", from.display());
        if !from.exists() {
            // Nothing to embed. `ffmpeg_runtime` falls back to whatever is
            // beside the executable, which is how a dev build works anyway.
            let _ = std::fs::write(&to, []);
            continue;
        }
        // Rebuilding a 63 MB compression on every `cargo build` would be
        // intolerable; the DLL only changes when ffmpeg is updated.
        if is_newer(&to, &from) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&from) else { continue };
        // Level 11: most of the ratio, without the minutes level 19 costs.
        match zstd::encode_all(&bytes[..], 11) {
            Ok(packed) => {
                let _ = std::fs::write(&to, packed);
            }
            Err(e) => println!("cargo:warning=could not compress {dll}: {e}"),
        }
    }

    // Identifies this exact set of DLLs, so an updated ffmpeg extracts to a new
    // directory instead of being mistaken for what is already cached there.
    // FNV over each file's length and first bytes: enough to separate builds,
    // and it does not read 66 MB to decide.
    for dll in RUNTIME_DLLS {
        let path = out.join(format!("{dll}.zst"));
        let Ok(bytes) = std::fs::read(&path) else { continue };
        for byte in bytes.len().to_le_bytes().iter().chain(bytes.iter().take(64)) {
            fingerprint ^= *byte as u64;
            fingerprint = fingerprint.wrapping_mul(0x100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=DB_RUNTIME_ID={fingerprint:016x}");
}
