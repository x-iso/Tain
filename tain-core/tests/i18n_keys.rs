//! Every key the source names has to exist in the catalogue.
//!
//! # Why this is a test and not a review
//!
//! A missing key is not a compile error and not a panic. It shows up as
//! `settings.video.lossless_cost` sitting in the middle of the settings window,
//! and only on the screen that happens to display it — which is how two
//! separate passes over this application still left English text on a Russian
//! settings panel and a warning key nothing ever asked for.
//!
//! So the check is mechanical: read every `.rs` file in the workspace, pull out
//! every string literal handed to `i18n::text`, `i18n::format`, `i18n::plural`
//! or `t!`, and confirm the English catalogue has it. English rather than every
//! language, because a translation may legitimately be unfinished — that is
//! what `every_language_reports_what_it_is_missing` prints — while a key no
//! catalogue has at all is a bug in this repository.
//!
//! Keys built at run time (`format!("end.{code}")`) are invisible here by
//! construction. The one that matters is covered by a test of its own beside
//! `explain_session_end`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every `.rs` file under the workspace's two crates.
///
/// Returns nothing at all when the sources are not beside the manifest — a
/// packaged crate, say — and the test then passes vacuously rather than failing
/// somewhere the check cannot run.
fn workspace_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let mut found = Vec::new();
    for crate_dir in ["tain-core/src", "tain-cli/src"] {
        collect(&root.join(crate_dir), &mut found);
    }
    found
}

fn collect(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // The one file whose "keys" are the machinery itself: `plural`
            // builds `{key}.{variant}` from a runtime string, and the tests
            // beside it look up keys that deliberately do not exist.
            if path.file_name().is_some_and(|n| n == "i18n.rs") {
                continue;
            }
            into.push(path);
        }
    }
}

/// Where the bracket opened at `open` closes, or the end of the file.
///
/// Counts brackets, and steps over string literals so that a `)` inside one
/// does not close anything.
fn closing(source: &str, open: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut at = open;
    while at < bytes.len() {
        match bytes[at] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return at;
                }
            }
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}

/// The key on the right of every `=>` in a match body.
fn arms(body: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut from = 0;
    while let Some(at) = body[from..].find("=>") {
        let after = from + at + 2;
        from = after;
        let rest = body[after..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else { continue };
        let Some(end) = literal.find('"') else { continue };
        let key = &literal[..end];
        if key.contains('.') {
            keys.push(key.to_string());
        }
    }
    keys
}

/// The keys one file asks for, as `(key, plural)`.
///
/// A plural key is stored without its variant: the catalogue holds `key.one`
/// and `key.other`, and `i18n::plural` appends whichever the language calls
/// for. `other` is the one every catalogue must have, so that is what gets
/// checked.
fn keys_in(source: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let bytes = source.as_bytes();

    for (call, plural) in [
        ("i18n::text(", false),
        ("i18n::format(", false),
        ("i18n::plural(", true),
        ("t!(", false),
    ] {
        let mut from = 0;
        while let Some(at) = source[from..].find(call) {
            let start = from + at;
            let after = start + call.len();
            from = after;

            // `t!(` is the tail of `format!(`, `assert!(` and every other
            // macro ending in a `t`. Only a call that starts on a word
            // boundary is the one meant here.
            if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
                continue;
            }

            let Some(offset) = bytes[after..].iter().position(|b| !b.is_ascii_whitespace()) else {
                continue;
            };
            let head = after + offset;

            // The other shape this file is full of: one call wrapping a match
            // whose arms are the keys. Every arm counts, and they end where the
            // call's own bracket closes.
            //
            //     i18n::text(match self {
            //         Self::Starting => "step.starting",
            //         ...
            //     })
            if source[head..].starts_with("match ") {
                let arm = arms(&source[head..closing(source, after - 1)]);
                found.extend(arm.into_iter().map(|key| (key, plural)));
                continue;
            }

            // Anything that is not a literal here is a key built from a
            // variable, which this check cannot see and does not try to.
            if bytes[head] != b'"' {
                continue;
            }
            let quote = head + 1;
            let Some(end) = source[quote..].find('"') else { continue };
            let key = &source[quote..quote + end];
            if key.is_empty() || !key.contains('.') {
                continue;
            }

            // `t!` takes a count too, and `t!("x", count = n)` is a plural.
            let rest = &source[quote + end..];
            let is_plural = plural
                || (call == "t!("
                    && rest
                        .split_once(')')
                        .is_some_and(|(args, _)| args.contains("count =")));
            found.push((key.to_string(), is_plural));
        }
    }
    found
}

#[test]
fn every_key_the_code_names_is_in_the_catalogue() {
    let sources = workspace_sources();
    if sources.is_empty() {
        eprintln!("no sources beside the manifest — nothing to check");
        return;
    }

    let mut absent: BTreeSet<String> = BTreeSet::new();
    let mut checked = 0usize;

    for path in &sources {
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        for (key, plural) in keys_in(&source) {
            checked += 1;
            // A plural key lives in the catalogue as its variants.
            let wanted = if plural { format!("{key}.other") } else { key.clone() };
            if !tain_core::i18n::has(&wanted) {
                let shown = path.file_name().unwrap_or_default().to_string_lossy();
                absent.insert(format!("{wanted}  ({shown})"));
            }
        }
    }

    assert!(checked > 100, "only found {checked} keys — the scan is not reading the sources");
    assert!(
        absent.is_empty(),
        "these keys are asked for but no catalogue has them, so they will \
         show on screen as themselves:\n  {}",
        absent.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

/// The reverse: text sitting in the catalogue that nothing displays.
///
/// Not a failure. A key can be perfectly alive and invisible here — built at
/// run time, or read by the loader itself — so this prints rather than
/// asserts, and exists to make a genuinely stale entry findable instead of
/// leaving it to be noticed by whoever next translates the file.
#[test]
fn text_nothing_asks_for_is_worth_knowing_about() {
    let sources = workspace_sources();
    if sources.is_empty() {
        return;
    }

    let mut asked: BTreeSet<String> = BTreeSet::new();
    for path in &sources {
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        for (key, plural) in keys_in(&source) {
            if plural {
                asked.insert(format!("{key}.one"));
                asked.insert(format!("{key}.other"));
                asked.insert(format!("{key}.few"));
                asked.insert(format!("{key}.many"));
            }
            asked.insert(key);
        }
    }

    // Built where they are used, not named as literals.
    let dynamic = ["meta.", "end."];

    let english = tain_core::i18n::languages()
        .iter()
        .find(|l| l.tag == "en")
        .expect("the English catalogue is compiled in");

    let orphans: Vec<&str> = english
        .keys()
        .into_iter()
        .filter(|key| !asked.contains(*key))
        .filter(|key| !dynamic.iter().any(|prefix| key.starts_with(prefix)))
        .collect();

    if !orphans.is_empty() {
        println!("{} catalogue entries nothing names:", orphans.len());
        for key in orphans {
            println!("    {key}");
        }
    }
}
