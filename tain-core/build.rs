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
    //
    // The first two belonged to a nasm check that was not recovered at all.
    // `check_nasm` is a rewrite, not a reconstruction: written to do what the
    // README says this script does, rather than remembered.
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    println!("cargo:rerun-if-env-changed=NASM");
    println!("cargo:rerun-if-changed=build.rs");

    embed_languages();
    check_nasm();

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
        // Named rather than merely described: a warning that says what is
        // missing without saying where it should be just moves the search
        // somewhere else.
        //
        // The build is committed in `third_party/ffmpeg`, so this is a checkout
        // without it, or an FFMPEG_DIR naming a folder that is not there. Not
        // fatal: ffmpeg is optional, everything below is skipped, and the crate
        // still builds and still decodes.
        println!(
            "cargo:warning=no ffmpeg tree found — building without hardware decode or audio. \
             It belongs in third_party/ffmpeg (scripts/build-ffmpeg.sh rebuilds it), \
             or set FFMPEG_DIR to an existing shared build."
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
                "cargo:warning=using ffmpeg's shipped import libraries — a missing DLL will \
                 fail before main() instead of falling back to the software decoder"
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

/// Compile every translation in `i18n/` into the binary.
///
/// # Why a directory rather than a list
///
/// Adding a language should be dropping a file in. A list in a Rust file would
/// mean every translator's first contribution touches code, needs a reviewer
/// who reads Rust, and can be rejected for a trailing comma — which is a
/// remarkable amount of friction to place in front of somebody volunteering to
/// translate an application.
///
/// So this globs the directory. The catalogue says its own tag, name and plural
/// rule; see `i18n/README.md`, which is written for the person adding one.
///
/// Nothing here can fail the build. A malformed catalogue is one language that
/// does not appear, reported as a warning — the alternative is a contributor's
/// typo breaking everybody else's build.
fn embed_languages() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../i18n");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut entries = Vec::new();
    match std::fs::read_dir(&dir) {
        Ok(found) => {
            let mut files: Vec<PathBuf> = found
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("lang"))
                })
                .collect();
            // Sorted, so the compiled order does not depend on the order a
            // directory happened to be read in.
            files.sort();
            for path in files {
                println!("cargo:rerun-if-changed={}", path.display());
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                entries.push((name, path));
            }
        }
        Err(e) => println!("cargo:warning=no i18n directory at {}: {e}", dir.display()),
    }

    if entries.is_empty() {
        println!("cargo:warning=no .lang catalogues found — the interface will be English only");
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut source = String::from(
        "/// Every translation compiled into this build, as (file name, contents).\n\
         pub static FILES: &[(&str, &str)] = &[\n",
    );
    for (name, path) in &entries {
        // `include_str!` rather than the text itself: the file stays a file,
        // errors point at it, and nothing has to be escaped into a Rust string.
        source.push_str(&format!(
            "    ({:?}, include_str!({:?})),\n",
            name,
            path.canonicalize().unwrap_or_else(|_| path.clone()),
        ));
    }
    source.push_str("];\n");

    let target = out.join("language_catalogues.rs");
    if let Err(e) = std::fs::write(&target, source) {
        println!("cargo:warning=could not write {}: {e}", target.display());
    }
}

/// Warn when `nasm` cannot be found, because nothing else will.
///
/// # The trap
///
/// `openh264-sys2` assembles OpenH264's SIMD kernels through `nasm-rs`, and
/// when that fails it does not fail the build. It prints "not using any
/// assembly" with a plain `println!` — which cargo keeps to itself unless asked
/// for `-vv` — and compiles plain C instead. Measured at 2560x1600 that is
/// 1147 ms a frame against 67 ms, about 17x, with no error anywhere and
/// symptoms that point at the network. "The nasm Trap" in docs/decisions.md
/// has the numbers.
///
/// # A prediction, not a report
///
/// This cannot see what `openh264-sys2` actually did. Its build script is a
/// separate process with no ordering against this one, and it has no `links`
/// key to hand anything back through. So this repeats nasm-rs 0.3.2's own
/// search, in the same environment, and expects the same answer: `NASM` if it
/// is set, and *only* that when it is — a stale `NASM` loses to a working nasm
/// on PATH, which is the case most worth catching — otherwise `nasm`, then
/// `nasm` in each PATH directory in turn. A candidate counts if `-v` runs and
/// succeeds. nasm-rs also parses the version against a minimum of 1.0, which no
/// nasm still in circulation fails.
///
/// Nor can it see nasm failing on the files themselves. nasm 3.02 segfaults on
/// half of OpenH264's common x86 files when given `-g`, which nasm-rs adds
/// whenever cargo sets `DEBUG`. Every dev-profile build used to come out plain
/// C that way, with nasm present and this silent, until the workspace
/// Cargo.toml turned debug info off for openh264-sys2 alone, which makes cargo
/// pass `DEBUG=false` to its build script. The release workflow reads
/// openh264-sys2's own build output instead, which is the authoritative check.
/// This is the one every local build gets.
///
/// # Quiet only once it is fixed
///
/// `openh264-sys2` declares no interest in `NASM` or PATH, so installing nasm
/// afterwards does not rebuild it: the plain-C build stays cached. Cargo
/// replays this warning from the cached output on every build until this
/// script runs again, so the warning asks for both crates to be cleaned. That
/// rebuilds OpenH264 with nasm and reruns this check, and the warning goes away
/// at the moment its reason does.
///
/// `NASM` is tracked anyway, as the recovered script did, so correcting it
/// reruns this check at once — before openh264-sys2 has been cleaned. That is
/// the one way for the warning to go quiet early, and why it spells out the
/// clean rather than leaving it to be guessed.
///
/// # Why PATH is not tracked
///
/// Partly the same reason: nasm arriving on PATH would quiet the warning over a
/// binary that is still slow. And `rerun-if-env-changed=PATH` would rerun this
/// script — and so recompile this crate and everything above it — whenever
/// cargo starts with a different PATH than last time. Git Bash hands cargo a
/// different PATH from PowerShell's (its own tool directories come first), a
/// Visual Studio developer prompt another, and an editor's cargo —
/// rust-analyzer's, into the same target directory — whatever the editor was
/// started with. Alternating between any two would recompile on every switch.
fn check_nasm() {
    // Asked for: openh264-sys2 skips assembly whenever this is set, so whether
    // nasm exists does not matter.
    if std::env::var("OPENH264_NO_ASM").is_ok() {
        return;
    }
    // openh264-sys2 only has assembly for x86 and x86-64, and Windows is the
    // platform the trap was found and measured on. The target's, not the
    // host's: `cfg!` in a build script describes the machine running it.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "windows" || !matches!(arch.as_str(), "x86_64" | "x86") {
        return;
    }

    // `var`, not `var_os`, as nasm-rs does: a NASM that is not valid Unicode
    // counts as unset there too.
    let nasm = std::env::var("NASM").ok();
    let candidates: Vec<PathBuf> = match &nasm {
        Some(path) => vec![PathBuf::from(path)],
        None => {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::iter::once(PathBuf::from("nasm"))
                .chain(std::env::split_paths(&path).map(|dir| dir.join("nasm")))
                .collect()
        }
    };

    let mut first_error = None;
    for candidate in &candidates {
        match std::process::Command::new(candidate).arg("-v").output() {
            Ok(out) if out.status.success() => return,
            Ok(out) => {
                first_error.get_or_insert_with(|| format!("`-v` exited with {}", out.status));
            }
            Err(e) => {
                first_error.get_or_insert_with(|| e.to_string());
            }
        }
    }
    let why = first_error.unwrap_or_default();

    // Says which lookup failed, since the fix differs: a NASM pointing at the
    // wrong place, or no nasm anywhere.
    let found = match &nasm {
        Some(path) => format!("NASM is set to {path}, which does not run ({why})"),
        None => format!("NASM is not set and there is no nasm on PATH ({why})"),
    };
    println!(
        "cargo:warning=nasm not found: {found}. Without it OpenH264 builds with no SIMD \
         assembly, about 17x slower per frame. Install nasm and point NASM or PATH at it \
         (see .cargo/config.toml.example), then run `cargo clean -p openh264-sys2 -p tain-core`: \
         a plain-C OpenH264 stays cached otherwise, and so does this warning."
    );
}
