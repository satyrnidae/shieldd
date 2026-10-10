//! UI text, from the Fluent files under locales/, one per language, embedded: the tray's and the
//! notices shieldd sends to it. CLI help, errors and logs stay English.
//!
//! The user's Windows display languages pick the text, most preferred first. Anything a language
//! leaves out falls back to en-US.

use std::sync::OnceLock;

use fluent_bundle::FluentResource;
use fluent_bundle::concurrent::FluentBundle;
use tracing::warn;
use unic_langid::LanguageIdentifier;

#[doc(hidden)]
pub use fluent_bundle::FluentArgs;

/// Every language there's text for, en-US first
const LOCALES: &[(&str, &str)] = &[("en-US", include_str!("../locales/en-US/shdtray.ftl"))];

/// The user's languages that have text, most preferred first, ending with en-US
static BUNDLES: OnceLock<Vec<FluentBundle<FluentResource>>> = OnceLock::new();

/// The text for a Fluent message, in the user's language: `tr!("menu-exit")`, or with
/// variables, `tr!("tip-line", name = n, port = p, level = l)`
#[macro_export]
macro_rules! tr {
    ($id:literal) => {
        $crate::l10n::text($id, None)
    };
    ($id:literal, $($key:ident = $value:expr),+ $(,)?) => {{
        let mut args = $crate::l10n::FluentArgs::new();
        $(args.set(stringify!($key), $value);)+
        $crate::l10n::text($id, Some(&args))
    }};
}

/// Message `id`, formatted with `args`, from the first language that has it. Its id if none do,
/// which a test makes sure of for every id in the source.
#[doc(hidden)]
pub fn text(id: &str, args: Option<&FluentArgs>) -> String {
    for b in BUNDLES.get_or_init(load) {
        let Some(pattern) = b.get_message(id).and_then(|m| m.value()) else { continue };
        let mut errors = Vec::new();
        let s = b.format_pattern(pattern, args, &mut errors);
        if !errors.is_empty() {
            warn!(id, ?errors, "text has errors");
        }
        return s.into_owned();
    }
    warn!(id, "no text");
    id.into()
}

fn load() -> Vec<FluentBundle<FluentResource>> {
    pick(&ui_languages(), LOCALES.iter().map(|(tag, _)| *tag)).into_iter().map(bundle).collect()
}

fn bundle(i: usize) -> FluentBundle<FluentResource> {
    let (tag, src) = LOCALES[i];
    let lang: LanguageIdentifier = tag.parse().expect("LOCALES tags are valid");
    let mut b = FluentBundle::new_concurrent(vec![lang]);
    // Bidi isolation marks around each variable; none of the languages so far are right to left
    b.set_use_isolating(false);
    let res = FluentResource::try_new(src.into()).unwrap_or_else(|(res, errors)| {
        warn!(tag, ?errors, "text doesn't parse; using what does");
        res
    });
    if let Err(errors) = b.add_resource(res) {
        warn!(tag, ?errors, "text has duplicates");
    }
    b
}

/// Indexes into `have`, for the languages in `wanted` it has, in that order: the same tag, or
/// else the same language in another region. Then the first of `have`, the fallback.
fn pick<'a>(wanted: &[String], have: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let tags: Vec<Option<LanguageIdentifier>> = have.map(|t| t.parse().ok()).collect();
    let mut out = Vec::new();
    for w in wanted {
        let Ok(w) = w.parse::<LanguageIdentifier>() else { continue };
        let exact = tags.iter().position(|t| t.as_ref() == Some(&w));
        let near = || tags.iter().position(|t| t.as_ref().is_some_and(|t| t.language == w.language));
        if let Some(i) = exact.or_else(near)
            && !out.contains(&i)
        {
            out.push(i);
        }
    }
    if !out.contains(&0) {
        out.push(0);
    }
    out
}

/// The user's display languages as BCP 47 tags, most preferred first
fn ui_languages() -> Vec<String> {
    #[cfg(windows)]
    let wanted = crate::windows::ui_languages();
    #[cfg(not(windows))]
    let wanted = Vec::new();
    // Tests compare English text, whatever the machine's set to
    if cfg!(test) { Vec::new() } else { wanted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn ids(src: &str) -> BTreeSet<String> {
        let res = FluentResource::try_new(src.into()).unwrap_or_else(|(_, e)| panic!("{e:?}"));
        res.entries()
            .filter_map(|e| match e {
                fluent_syntax::ast::Entry::Message(m) => Some(m.id.name.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Every tr! id in `dir`'s Rust files, recursively
    fn used(dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                used(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let src = std::fs::read_to_string(&p).unwrap();
                for (at, _) in src.match_indices("tr!(") {
                    // Not the tail of include_str!( and the like, nor this pattern
                    if src[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '"') {
                        continue;
                    }
                    let Some(rest) = src[at + 4..].trim_start().strip_prefix('"') else { continue };
                    out.insert(rest[..rest.find('"').unwrap()].into());
                }
            }
        }
    }

    #[test]
    fn every_id_is_there_and_used() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut in_source = BTreeSet::new();
        used(&root.join("src"), &mut in_source);
        used(&root.join("../shieldd/src"), &mut in_source);
        let english = ids(LOCALES[0].1);
        let missing: Vec<_> = in_source.difference(&english).collect();
        assert!(missing.is_empty(), "no text for {missing:?}");
        let unused: Vec<_> = english.difference(&in_source).collect();
        assert!(unused.is_empty(), "unused text {unused:?}");
    }

    #[test]
    fn other_languages_only_have_english_ids() {
        let english = ids(LOCALES[0].1);
        for (tag, src) in &LOCALES[1..] {
            let stale: Vec<_> = ids(src).difference(&english).cloned().collect();
            assert!(stale.is_empty(), "{tag} has {stale:?}, which English doesn't");
        }
    }

    /// With every variable any message takes, each formats without an error
    #[test]
    fn english_formats_clean() {
        let b = bundle(0);
        let mut args = FluentArgs::new();
        for k in ["version", "ssid", "state", "name", "port", "mac", "level", "built", "author", "error"] {
            args.set(k, "x");
        }
        for k in ["count", "channel", "pct", "max", "mic_stream", "mic_port", "pad_stream", "pad_port"] {
            args.set(k, 2);
        }
        for id in ids(LOCALES[0].1) {
            let mut errors = Vec::new();
            let p = b.get_message(&id).and_then(|m| m.value()).unwrap();
            let s = b.format_pattern(p, Some(&args), &mut errors);
            assert!(errors.is_empty(), "{id}: {errors:?} in {s:?}");
        }
        assert_eq!(tr!("rename-prompt", mac = "m", max = 1), "Name for m, up to 1 character:");
        assert_eq!(tr!("menu-connected", count = 3), "3 connected.");
        assert_eq!(tr!("controller-line", state = "level", name = "A", port = "?", pct = 7), "A: Port #? (7%)");
        assert_eq!(tr!("tip-line", name = "A", port = 1, level = "8%"), "A Port #1: 8%");
        assert_eq!(tr!("tip-line", name = "A", port = "none", level = "8%"), "A: 8%");
    }

    #[test]
    fn picks_exact_then_same_language_then_english() {
        let have = ["en-US", "de-DE", "fr-FR", "fr-CA"];
        let pick = |w: &[&str]| pick(&w.iter().map(|s| s.to_string()).collect::<Vec<_>>(), have.iter().copied());
        assert_eq!(pick(&[]), [0]);
        assert_eq!(pick(&["fr-CA", "de-AT"]), [3, 1, 0]);
        assert_eq!(pick(&["fr-BE"]), [2, 0]);
        assert_eq!(pick(&["en-GB", "de-DE"]), [0, 1]);
        assert_eq!(pick(&["xx-not a tag", "ja-JP"]), [0]);
    }
}
