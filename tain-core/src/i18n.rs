//! What the application says, in the language of whoever is reading it.
//!
//! # The shape of it
//!
//! One file per language in `i18n/`, picked up by `build.rs` and compiled into
//! the binary. A language is added by dropping a file in — there is no list to
//! join and no code to edit, because a contribution that needs a Rust change is
//! a contribution that needs a reviewer who can read Rust.
//!
//! The format is deliberately not TOML, JSON or YAML. Every one of those has a
//! quoting rule, and the text being translated is full of apostrophes,
//! quotation marks and em dashes — so the first thing a translator would hit is
//! a syntax error in a language they are not writing. See `i18n/README.md`; the
//! whole grammar is "a key in brackets, then its text".
//!
//! # Missing keys fall back rather than fail
//!
//! An unfinished translation shows English for what it has not reached yet.
//! The alternative — blank labels, or refusing to build — makes the first
//! contribution the hardest one, when it should be the easiest. What a
//! maintainer needs instead is to *know*, which is what
//! [`missing_from`] and the test beside it are for.
//!
//! # What is not translated
//!
//! Log output. Every diagnosis in this project's history has begun with a log
//! somebody sent, and one written in a language the maintainer cannot read is a
//! materially harder problem. `tracing` stays English on purpose.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The catalogues, as `(file name, contents)`, compiled in by `build.rs`.
mod catalogues {
    include!(concat!(env!("OUT_DIR"), "/language_catalogues.rs"));
}

/// How a language picks between plural forms.
///
/// Named in each catalogue rather than inferred from the tag, because a
/// contributor knows their own grammar and a lookup table of every language's
/// rules is a thing to get wrong on somebody else's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Plural {
    /// One, and everything else. English, German, Spanish, most of Europe.
    #[default]
    English,
    /// One, a few (2–4), many (5+), by the last digits. Russian, Ukrainian.
    Russian,
    /// A single form. Chinese, Japanese, Korean, Turkish.
    None,
}

impl Plural {
    fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "russian" => Self::Russian,
            "none" => Self::None,
            // Includes anything unrecognised: `other` for every count is wrong
            // in some language, and readable in all of them.
            _ => Self::English,
        }
    }

    /// Which variant of a key a count wants.
    ///
    /// The Russian rule is the CLDR one, and it is not "one, two, many": 21 is
    /// *one*, 22 is *few*, and 11 through 14 are *many* whatever their last
    /// digit says. Written out because getting it wrong reads as illiteracy
    /// rather than as a bug.
    pub fn variant(self, n: u64) -> &'static str {
        match self {
            Self::None => "other",
            Self::English => {
                if n == 1 {
                    "one"
                } else {
                    "other"
                }
            }
            Self::Russian => {
                let (last, last_two) = (n % 10, n % 100);
                if last == 1 && last_two != 11 {
                    "one"
                } else if (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
                    "few"
                } else {
                    "many"
                }
            }
        }
    }
}

/// One language the application can speak.
pub struct Language {
    /// BCP 47 tag, e.g. `en`, `ru`, `pt-BR`.
    pub tag: String,
    /// What the language calls itself. Nobody looks for their own language
    /// under an English name.
    pub name: String,
    pub plural: Plural,
    entries: HashMap<String, String>,
}

impl Language {
    /// Every key this catalogue defines, in order.
    ///
    /// Sorted so that anything reporting on it — the completeness check, the
    /// test that looks for entries nothing displays — reads the same twice.
    pub fn keys(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        all.sort_unstable();
        all
    }

    /// This catalogue's text for a key, with no fallback to anywhere else.
    ///
    /// [`text`] is what displays a key; this answers what one *particular*
    /// language says, which is a different question and the one a completeness
    /// check or a test comparing two catalogues is asking.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }
}

/// Split a catalogue into its entries.
///
/// Pure, and the only thing that understands the file format — so the format is
/// pinned by tests rather than discovered by a translator whose file silently
/// did nothing.
///
/// A key is a line that is exactly `[some.key]`. Everything up to the next key
/// is its text, trimmed of surrounding blank lines but not of the line breaks
/// inside it: a paragraph stays a paragraph.
pub fn parse(catalogue: &str) -> HashMap<String, String> {
    let mut entries = HashMap::new();
    let mut key: Option<String> = None;
    let mut value = String::new();

    let mut finish = |key: &mut Option<String>, value: &mut String| {
        if let Some(key) = key.take() {
            entries.insert(key, value.trim_matches('\n').trim_end().to_string());
        }
        value.clear();
    };

    for line in catalogue.lines() {
        let trimmed = line.trim();
        if let Some(name) = key_line(trimmed) {
            finish(&mut key, &mut value);
            key = Some(name.to_string());
            continue;
        }
        // Comments only at the start of a line, so a `#` inside a sentence is
        // just a `#`.
        if trimmed.starts_with('#') {
            continue;
        }
        if key.is_some() {
            value.push_str(line.trim_end());
            value.push('\n');
        }
    }
    finish(&mut key, &mut value);
    entries
}

/// `[some.key]` and nothing else on the line.
///
/// Strict on purpose: a line of translated text that happens to start with a
/// bracket must not become a key, and a key with a space or a capital in it is
/// a typo that should show up as untranslated rather than as a silent miss.
fn key_line(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let usable = !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
    usable.then_some(inner)
}

/// Every language compiled into this build, English first.
pub fn languages() -> &'static [Language] {
    static ALL: OnceLock<Vec<Language>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut found: Vec<Language> = catalogues::FILES
            .iter()
            .filter_map(|(name, text)| {
                let entries = parse(text);
                let tag = entries.get("meta.tag").cloned().or_else(|| {
                    tracing::warn!("{name} has no meta.tag — ignoring it");
                    None
                })?;
                Some(Language {
                    name: entries.get("meta.name").cloned().unwrap_or_else(|| tag.clone()),
                    plural: entries.get("meta.plural").map_or(Plural::English, |p| {
                        Plural::parse(p)
                    }),
                    tag,
                    entries,
                })
            })
            .collect();
        // English first because it is the fallback and the one a picker should
        // open on; the rest by name, so the list does not depend on the order a
        // directory happened to be read in.
        found.sort_by(|a, b| match (a.tag == "en", b.tag == "en") {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        found
    })
}

/// The one everything falls back to.
fn english() -> Option<&'static Language> {
    languages().iter().find(|l| l.tag == "en")
}

/// The language in force, or `None` before one has been chosen.
static ACTIVE: RwLock<Option<usize>> = RwLock::new(None);

/// Which language is being spoken.
pub fn active() -> Option<&'static Language> {
    let index = *ACTIVE.read().unwrap_or_else(|e| e.into_inner());
    index.and_then(|index| languages().get(index))
}

/// Speak this language from now on, by tag. Returns whether it was found.
pub fn use_language(tag: &str) -> bool {
    let Some(index) = best_match(tag) else { return false };
    let mut active = ACTIVE.write().unwrap_or_else(|e| e.into_inner());
    // Said only when it changes. This is called on every frame in which a
    // setting was touched — dragging any slider in the window reaches it — and
    // a line per frame would bury everything else in the log.
    if *active != Some(index) {
        let language = &languages()[index];
        tracing::info!("Interface language: {} ({})", language.name, language.tag);
    }
    *active = Some(index);
    true
}

/// Speak the language the setting names, the system's, or English.
///
/// `preference` empty means "follow the computer", which is the default: an
/// application that opens in a language its user does not read is one they have
/// to guess their way through to the setting that fixes it.
///
/// Falls all the way through to English rather than failing, because there is
/// no useful behaviour on the other side of "your language is not available".
pub fn follow(preference: &str) {
    if !preference.trim().is_empty() && use_language(preference) {
        return;
    }
    if let Some(system) = system_language() {
        if use_language(&system) {
            return;
        }
        tracing::debug!("No catalogue for the system language {system:?} — using English");
    }
    use_language("en");
}

/// What "Automatic" would choose right now, for the picker to name.
pub fn automatic() -> Option<&'static Language> {
    let system = system_language()?;
    best_match(&system).and_then(|index| languages().get(index))
}

/// The best catalogue for a system language tag.
///
/// Exact first, then the base language: Windows says `pt-BR`, and a `pt`
/// catalogue should answer for it — while a `pt-BR` catalogue, if somebody
/// contributes one, should win over `pt`. Pure, so the matching can be argued
/// about in a test rather than on somebody's Brazilian laptop.
pub fn best_match(tag: &str) -> Option<usize> {
    let wanted = tag.trim().to_ascii_lowercase().replace('_', "-");
    if wanted.is_empty() {
        return None;
    }
    let all = languages();

    if let Some(exact) = all.iter().position(|l| l.tag.eq_ignore_ascii_case(&wanted)) {
        return Some(exact);
    }
    // `pt-BR` asked for, only `pt` available.
    let base = wanted.split('-').next().unwrap_or(&wanted);
    if let Some(by_base) = all.iter().position(|l| l.tag.eq_ignore_ascii_case(base)) {
        return Some(by_base);
    }
    // `pt` asked for, only `pt-BR` available. Better than English, and the only
    // remaining reading of the request.
    let prefix = format!("{base}-");
    all.iter().position(|l| l.tag.to_ascii_lowercase().starts_with(&prefix))
}

/// Look up a key, falling back to English and then to the key itself.
///
/// The key is what shows on screen when nothing has it, which is deliberate: a
/// blank label is a mystery, and `settings.encoder.title` is a bug report.
pub fn text(key: &str) -> &'static str {
    let found = active()
        .and_then(|language| language.get(key))
        .or_else(|| english().and_then(|language| language.get(key)));
    match found {
        Some(text) => text,
        None => {
            missed(key);
            // Leaked deliberately and once per unknown key: this is a
            // programming mistake, the set of them is finite and small, and the
            // alternative is threading a lifetime through every call site to
            // carry a string that should not exist.
            Box::leak(key.to_string().into_boxed_str())
        }
    }
}

/// The English text for a key, whatever language the interface is in.
///
/// For the log, and only for the log. Diagnosing a machine starts with the file
/// its owner sends, and one written in a language the maintainer cannot read is
/// materially harder to help with — so a sentence that is shown translated and
/// *also* logged is looked up twice, once each way. See `i18n/README.md`, which
/// says the same thing to translators.
pub fn in_english(key: &str) -> &'static str {
    english()
        .and_then(|language| language.get(key))
        .unwrap_or_else(|| Box::leak(key.to_string().into_boxed_str()))
}

/// [`in_english`], with `{name}` placeholders substituted.
pub fn format_in_english(key: &str, args: &[(&str, &str)]) -> String {
    substitute(in_english(key), args)
}

/// Whether any catalogue has this key.
///
/// The one thing that needs asking is a code sent by another machine, which may
/// be a newer version carrying codes this one has never heard of. Every other
/// lookup is for a key written in this repository, where a miss is a bug and
/// [`text`]'s loud fallback is exactly what should happen.
pub fn has(key: &str) -> bool {
    active().and_then(|language| language.get(key)).is_some()
        || english().and_then(|language| language.get(key)).is_some()
}

/// Say once that a key is not in any catalogue.
fn missed(key: &str) {
    static SAID: OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let said = SAID.get_or_init(Default::default);
    let mut said = said.lock().unwrap_or_else(|e| e.into_inner());
    if said.insert(key.to_string()) {
        tracing::warn!("No text for {key:?} in any language — showing the key");
    }
}

/// Look up a key and substitute `{name}` placeholders.
pub fn format(key: &str, args: &[(&str, &str)]) -> String {
    substitute(text(key), args)
}

/// Look up the variant of `key` that `n` calls for, and substitute `{n}`.
///
/// The variant is chosen by the *active* language's rule, not English's — which
/// is the whole point, and the reason a count cannot simply be pasted into one
/// sentence with an `(s)` on the end.
pub fn plural(key: &str, n: u64, args: &[(&str, &str)]) -> String {
    let rule = active().map_or(Plural::English, |language| language.plural);
    let count = n.to_string();

    // The chosen variant, then `other`, which every catalogue must have.
    let variant = format!("{key}.{}", rule.variant(n));
    let template = active()
        .and_then(|language| language.get(&variant))
        .or_else(|| active().and_then(|language| language.get(&format!("{key}.other"))))
        .or_else(|| english().and_then(|language| language.get(&variant)))
        .map(str::to_string)
        .unwrap_or_else(|| text(&format!("{key}.other")).to_string());

    let mut with_count: Vec<(&str, &str)> = vec![("n", count.as_str())];
    with_count.extend_from_slice(args);
    substitute(&template, &with_count)
}

/// Replace `{name}` with what it stands for.
///
/// Anything unrecognised is left exactly as written, so a typo appears on
/// screen as `{nmae}` — visible, searchable, and obviously wrong — rather than
/// vanishing into a gap nobody notices.
pub fn substitute(template: &str, args: &[(&str, &str)]) -> String {
    if args.is_empty() || !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match args.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            // An unclosed brace is text.
            None => {
                out.push('{');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Keys English has that `tag` does not.
///
/// What a maintainer needs in order to say what a translation is still missing,
/// and what the test below reports. Sorted, so a list is comparable between
/// runs.
pub fn missing_from(tag: &str) -> Vec<String> {
    let (Some(english), Some(other)) =
        (english(), languages().iter().find(|l| l.tag.eq_ignore_ascii_case(tag)))
    else {
        return Vec::new();
    };
    let mut missing: Vec<String> = english
        .entries
        .keys()
        .filter(|key| !other.entries.contains_key(*key))
        .cloned()
        .collect();
    missing.sort();
    missing
}

/// Say the text for `key`, in the language in force.
///
/// ```ignore
/// t!("approval.allow")                       // plain
/// t!("incoming.name", name = guest)          // {name}
/// t!("incoming.sessions", count = n)         // plural, and {n}
/// ```
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        $crate::i18n::text($key)
    };
    ($key:expr, count = $n:expr) => {
        $crate::i18n::plural($key, ($n) as u64, &[])
    };
    ($key:expr, count = $n:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::plural($key, ($n) as u64, &[$((stringify!($name), &$value.to_string())),+])
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::format($key, &[$((stringify!($name), $value.to_string().as_str())),+])
    };
}

// ── Which language the machine is set to ──────────────────────

/// The system's preferred interface language, e.g. `ru-RU`.
///
/// `None` when it cannot be read, which is not a failure — it means English,
/// the same as an unrecognised one.
pub fn system_language() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::system_language()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // The convention everywhere else, and harmless where it is unset.
        std::env::var("LANG").ok().and_then(|value| {
            let tag = value.split(['.', '@']).next().unwrap_or_default().replace('_', "-");
            (!tag.is_empty() && tag != "C" && tag != "POSIX").then_some(tag)
        })
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    /// What Windows says this user wants to read.
    ///
    /// `GetUserPreferredUILanguages` rather than the locale: the locale is about
    /// dates and decimal points, and somebody in Germany reading English
    /// software has a German locale and an English interface. The *first*
    /// preferred UI language is the question being asked.
    pub fn system_language() -> Option<String> {
        use windows::Win32::Globalization::{
            GetUserPreferredUILanguages, MUI_LANGUAGE_NAME,
        };

        let mut count = 0u32;
        let mut length = 0u32;
        // SAFETY: asking for the buffer size, which is what a null buffer with
        // a valid length out-parameter means here.
        let sized = unsafe {
            GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut count,
                windows::core::PWSTR::null(),
                &mut length,
            )
        };
        if sized.is_err() || length == 0 {
            return None;
        }

        let mut buffer = vec![0u16; length as usize];
        // SAFETY: the buffer is exactly the length just asked for.
        let read = unsafe {
            GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut count,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        if read.is_err() {
            return None;
        }

        // A double-null-terminated list of names; the first is the preferred
        // one and the only one being asked about.
        let first: Vec<u16> = buffer.into_iter().take_while(|&c| c != 0).collect();
        let tag = String::from_utf16_lossy(&first);
        (!tag.is_empty()).then_some(tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole grammar, pinned.
    #[test]
    fn a_catalogue_is_keys_and_the_text_after_them() {
        let parsed = parse(
            "# a comment\n\
             [one.key]\n\
             first\n\
             \n\
             [two.key]\n\
             a line\n\
             and another\n",
        );
        assert_eq!(parsed.get("one.key").map(String::as_str), Some("first"));
        // Line breaks inside a value are the translator's, and are kept.
        assert_eq!(parsed.get("two.key").map(String::as_str), Some("a line\nand another"));
        assert_eq!(parsed.len(), 2, "the comment must not have become an entry");
    }

    /// Nothing in a translation can break the file.
    ///
    /// This is the reason the format is not TOML or JSON: a translator writing
    /// an apostrophe, a quotation mark or a backslash would hit a syntax error
    /// in a language they are not writing.
    #[test]
    fn punctuation_in_a_translation_is_just_punctuation() {
        let parsed = parse(
            "[quotes]\n\
             He said \"don't\" — it's a path: C:\\Users\\{name}\n",
        );
        assert_eq!(
            parsed.get("quotes").map(String::as_str),
            Some("He said \"don't\" — it's a path: C:\\Users\\{name}")
        );
    }

    /// A line of text that starts with a bracket is text.
    #[test]
    fn only_a_well_formed_key_line_is_a_key() {
        assert_eq!(key_line("[a.key]"), Some("a.key"));
        assert_eq!(key_line("[with_underscores.2]"), Some("with_underscores.2"));
        for not_a_key in ["[Shift] to continue", "[a key]", "[]", "[UPPER]", "text", "[unclosed"] {
            assert_eq!(key_line(not_a_key), None, "{not_a_key:?} was taken for a key");
        }
    }

    #[test]
    fn placeholders_are_replaced_by_name_not_by_order() {
        assert_eq!(
            substitute("{second} then {first}", &[("first", "A"), ("second", "B")]),
            "B then A"
        );
        // Twice, because a language may need it twice.
        assert_eq!(substitute("{x} and {x}", &[("x", "q")]), "q and q");
    }

    /// A typo has to be visible, not invisible.
    #[test]
    fn an_unknown_placeholder_is_left_on_screen() {
        assert_eq!(substitute("hello {nmae}", &[("name", "x")]), "hello {nmae}");
        assert_eq!(substitute("a { b", &[("x", "y")]), "a { b");
    }

    /// The Russian rule is not "one, two, many", and getting it wrong reads as
    /// illiteracy rather than as a bug.
    #[test]
    fn the_russian_plural_rule_is_the_real_one() {
        let ru = Plural::Russian;
        for one in [1, 21, 101, 1031] {
            assert_eq!(ru.variant(one), "one", "{one}");
        }
        for few in [2, 3, 4, 22, 34, 102] {
            assert_eq!(ru.variant(few), "few", "{few}");
        }
        // The teens are the trap: 11 is not "one" and 12 is not "few".
        for many in [0, 5, 11, 12, 13, 14, 15, 25, 100, 111] {
            assert_eq!(ru.variant(many), "many", "{many}");
        }
    }

    #[test]
    fn english_and_single_form_rules() {
        assert_eq!(Plural::English.variant(1), "one");
        for other in [0, 2, 11, 21] {
            assert_eq!(Plural::English.variant(other), "other", "{other}");
        }
        for any in [0, 1, 2, 5, 21] {
            assert_eq!(Plural::None.variant(any), "other", "{any}");
        }
    }

    /// A machine set to `pt-BR` must not fall through to English because only a
    /// `pt` catalogue exists.
    #[test]
    fn a_regional_tag_finds_its_base_language() {
        // Against whatever is actually compiled in, so this follows the real
        // catalogue set rather than a fixture that could drift from it.
        assert!(best_match("en").is_some(), "English must always be present");
        assert!(best_match("en-GB").is_some(), "a region must fall back to its language");
        assert!(best_match("EN-us").is_some(), "matching is case-insensitive");
        assert!(best_match("en_US").is_some(), "an underscore is a hyphen");
        assert!(best_match("zz").is_none(), "an unknown language is not a match");
        assert!(best_match("").is_none());
    }

    /// Every language ships the keys English has, or says which it is missing.
    ///
    /// Not a failure — an unfinished translation is usable and welcome, and the
    /// point of this is to *say* what is left rather than to refuse it.
    #[test]
    fn every_language_reports_what_it_is_missing() {
        let english_keys = english().map(|l| l.entries.len()).unwrap_or(0);
        assert!(english_keys > 0, "the English catalogue is empty or was not compiled in");

        for language in languages() {
            if language.tag == "en" {
                continue;
            }
            let missing = missing_from(&language.tag);
            let done = english_keys - missing.len();
            println!(
                "{} ({}): {done}/{english_keys} keys",
                language.name, language.tag
            );
            for key in missing.iter().take(40) {
                println!("    missing: {key}");
            }
            if missing.len() > 40 {
                println!("    ... and {} more", missing.len() - 40);
            }
        }
    }

    /// Every catalogue has to say what it is.
    #[test]
    fn every_catalogue_describes_itself() {
        for language in languages() {
            assert!(!language.tag.is_empty());
            assert!(!language.name.is_empty(), "{} has no meta.name", language.tag);
            assert!(
                language.entries.contains_key("meta.plural"),
                "{} does not say which plural rule it uses",
                language.tag
            );
        }
    }
}
