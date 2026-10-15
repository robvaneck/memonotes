use std::collections::HashMap;
use std::sync::OnceLock;

const TRANSLATIONS: &str = include_str!("../translations.json");
pub const LANGUAGES: [&str; 5] = ["en", "de", "fr", "nl", "es"];
type Catalog = HashMap<String, HashMap<String, String>>;
static PREFERRED_LANGUAGE: OnceLock<Option<&'static str>> = OnceLock::new();

pub fn initialize_language(preference: Option<&str>) {
    let preferred = preference.and_then(|preference| {
        LANGUAGES
            .iter()
            .copied()
            .find(|language| *language == preference)
    });
    let _ = PREFERRED_LANGUAGE.set(preferred);
}

pub fn startup_language_preference() -> Option<&'static str> {
    PREFERRED_LANGUAGE.get().copied().flatten()
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(TRANSLATIONS).expect("valid translation catalog"))
}

pub fn language_from_locales(locales: &str) -> &'static str {
    for locale in locales.split(':') {
        let language = locale.split(['_', '-', '.', '@']).next().unwrap_or("");
        if let Some(supported) = LANGUAGES
            .iter()
            .find(|supported| language.eq_ignore_ascii_case(supported))
        {
            return supported;
        }
        if matches!(language, "C" | "POSIX") {
            return "en";
        }
    }
    "en"
}

pub fn language() -> &'static str {
    static LANGUAGE: OnceLock<&'static str> = OnceLock::new();
    let environment_language = *LANGUAGE.get_or_init(|| {
        let variable = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
        resolve_language(
            variable("MEMONOTES_LANGUAGE").as_deref(),
            variable("LANGUAGE").as_deref(),
            variable("LC_ALL").as_deref(),
            variable("LC_MESSAGES").as_deref(),
            variable("LANG").as_deref(),
        )
    });
    language_with_preference(startup_language_preference(), environment_language)
}

fn language_with_preference(
    preference: Option<&str>,
    environment_language: &'static str,
) -> &'static str {
    preference
        .and_then(|preference| {
            LANGUAGES
                .iter()
                .copied()
                .find(|language| *language == preference)
        })
        .unwrap_or(environment_language)
}

fn resolve_language(
    override_locale: Option<&str>,
    preferences: Option<&str>,
    all: Option<&str>,
    messages: Option<&str>,
    lang: Option<&str>,
) -> &'static str {
    if let Some(locale) = override_locale {
        return language_from_locales(locale);
    }
    let locale = all.or(messages).or(lang).unwrap_or("C");
    let base = locale.split(['.', '@']).next().unwrap_or(locale);
    if matches!(base, "C" | "POSIX") {
        return "en";
    }
    language_from_locales(preferences.unwrap_or(locale))
}

pub fn translate_in<'a>(language: &str, key: &'a str) -> &'a str {
    catalog()
        .get(key)
        .and_then(|translations| {
            translations
                .get(language)
                .or_else(|| translations.get("en"))
        })
        .map_or(key, String::as_str)
}

pub fn tr(key: &str) -> &str {
    translate_in(language(), key)
}

pub fn tr_format(key: &str, values: &[(&str, String)]) -> String {
    format_template(tr(key), values)
}

fn format_template(template: &str, values: &[(&str, String)]) -> String {
    let mut result = String::new();
    let mut remainder = template;
    while let Some(start) = remainder.find('{') {
        result.push_str(&remainder[..start]);
        let Some(end) = remainder[start..].find('}') else {
            result.push_str(&remainder[start..]);
            return result;
        };
        let placeholder = &remainder[start + 1..start + end];
        let name = placeholder.split(':').next().unwrap_or(placeholder);
        if let Some((_, value)) = values.iter().find(|(candidate, _)| *candidate == name) {
            result.push_str(value);
        } else {
            result.push_str(&remainder[start..=start + end]);
        }
        remainder = &remainder[start + end + 1..];
    }
    result.push_str(remainder);
    result
}

#[macro_export]
macro_rules! tr_format {
    ($key:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::tr_format($key, &[$((stringify!($name), ($value).to_string())),*])
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(text: &str) -> BTreeSet<&str> {
        text.split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            .collect()
    }

    #[test]
    fn translations_are_complete() {
        assert!(!catalog().is_empty());
        for (key, translations) in catalog() {
            assert_eq!(translations.len(), LANGUAGES.len(), "{key}");
            let english = translations
                .get("en")
                .expect("English reference translation");
            for language in LANGUAGES {
                let value = translations
                    .get(language)
                    .unwrap_or_else(|| panic!("missing {language}: {key}"));
                assert!(!value.trim().is_empty(), "empty {language}: {key}");
                assert_eq!(
                    placeholders(english),
                    placeholders(value),
                    "{language}: {key}"
                );
            }
        }
    }

    #[test]
    fn translation_keys_use_semantic_identifiers() {
        for key in catalog().keys() {
            assert!(
                key.starts_with("color_") || key.contains('.'),
                "missing namespace: {key}"
            );
            for segment in key.split('.') {
                assert!(
                    segment
                        .bytes()
                        .next()
                        .is_some_and(|byte| byte.is_ascii_lowercase()),
                    "invalid key segment: {key}"
                );
                assert!(
                    segment.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                    "invalid key: {key}"
                );
            }
        }
        assert_eq!(translate_in("en", "color_mint"), "Mint");
        assert_eq!(translate_in("en", "color_iris"), "Iris");
        assert_eq!(translate_in("en", "color_teal"), "Teal");
        assert_eq!(
            translate_in("en", "api.memos.error.invalid_server_url"),
            "Invalid Memos server URL"
        );
    }

    #[test]
    fn resolves_system_locales_and_fallbacks() {
        for (locale, expected) in [
            ("en_US.UTF-8", "en"),
            ("de-DE", "de"),
            ("fr_FR", "fr"),
            ("nl_NL@euro", "nl"),
            ("es_ES.UTF-8", "es"),
            ("it:fr:en", "fr"),
            ("C.UTF-8", "en"),
            ("POSIX", "en"),
            ("ja_JP", "en"),
        ] {
            assert_eq!(language_from_locales(locale), expected);
        }
        assert_eq!(translate_in("de", "note.new"), "Neue Notiz");
        assert_eq!(translate_in("unknown", "note.new"), "New note");
        assert_eq!(translate_in("en", "unknown key"), "unknown key");
    }

    #[test]
    fn respects_locale_precedence() {
        assert_eq!(
            resolve_language(Some("es"), Some("nl"), Some("C"), None, None),
            "es"
        );
        assert_eq!(
            resolve_language(None, Some("nl"), Some("C.UTF-8"), None, Some("de_DE")),
            "en"
        );
        assert_eq!(
            resolve_language(None, Some("it:fr"), Some("de_DE"), None, None),
            "fr"
        );
        assert_eq!(
            resolve_language(None, None, Some("de_DE"), Some("nl_NL"), Some("fr_FR")),
            "de"
        );
        assert_eq!(
            resolve_language(None, None, None, Some("nl_NL"), Some("fr_FR")),
            "nl"
        );
        assert_eq!(
            resolve_language(None, None, None, None, Some("fr_FR")),
            "fr"
        );
        assert_eq!(resolve_language(None, None, None, None, None), "en");
    }

    #[test]
    fn saved_language_precedes_environment_and_system_default_restores_it() {
        for language in LANGUAGES {
            assert_eq!(language_with_preference(Some(language), "en"), language);
        }
        assert_eq!(language_with_preference(None, "de"), "de");
        assert_eq!(language_with_preference(Some("unknown"), "fr"), "fr");
    }

    #[test]
    fn interpolates_without_reinterpreting_values() {
        for language in LANGUAGES {
            let key = "api.memos.error.account_upload_failed";
            let translated = format_template(
                translate_in(language, key),
                &[
                    ("account", "Work {error:#}".to_owned()),
                    ("error", "HTTP 503".to_owned()),
                ],
            );
            assert!(translated.contains("Work {error:#}"));
            assert!(translated.contains("HTTP 503"));
            assert!(!translated.contains("{account}"));
        }
        assert_eq!(
            crate::tr_format!("link.open", title = "GitHub"),
            format_template(tr("link.open"), &[("title", "GitHub".to_owned())])
        );
        assert_eq!(
            format_template(
                "{second} / {first}",
                &[("first", "1".to_owned()), ("second", "2".to_owned())]
            ),
            "2 / 1"
        );
    }

    #[test]
    fn application_translation_keys_exist() {
        for source in [
            include_str!("main.rs"),
            include_str!("editor_table.rs"),
            include_str!("lib.rs"),
            include_str!("adapters.rs"),
            include_str!("credentials.rs"),
            include_str!("memos.rs"),
            include_str!("bin/memos_api_probe.rs"),
        ] {
            for marker in ["tr(", "tr_format!("] {
                for fragment in source.split(marker).skip(1) {
                    let Some(literal) = fragment.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    let key = literal.split('"').next().unwrap();
                    assert!(catalog().contains_key(key), "missing translation: {key}");
                }
            }
        }
        for key in [
            "color_default",
            "color_gray",
            "color_mauve",
            "color_slate",
            "color_sage",
            "color_olive",
            "color_sand",
            "color_tomato",
            "color_red",
            "color_ruby",
            "color_crimson",
            "color_pink",
            "color_plum",
            "color_purple",
            "color_violet",
            "color_iris",
            "color_indigo",
            "color_blue",
            "color_cyan",
            "color_teal",
            "color_jade",
            "color_green",
            "color_grass",
            "color_bronze",
            "color_gold",
            "color_brown",
            "color_orange",
            "color_amber",
            "color_yellow",
            "color_lime",
            "color_mint",
            "color_sky",
        ] {
            assert!(catalog().contains_key(key), "missing color: {key}");
        }
    }
}
