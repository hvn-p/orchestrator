//! The coordinator's language: a language tag, such as `fr`, `en` or
//! `pt-BR`. The setup conversation starts in the system's language, read from
//! the locale, and the user's choice is kept in the configuration; every
//! coordinator then writes its replies, journal notes and messages in it.
//! What code writes, admission's notices among them, stays in English.

use anyhow::{Result, ensure};

/// The language when nothing says otherwise.
pub const DEFAULT: &str = "en";

/// Whether `tag` is a language tag orchestrator accepts: a primary subtag of
/// two or three lowercase letters, then up to three subtags of two to eight
/// letters or digits, joined by `-`, as in BCP 47: `fr`, `en`, `pt-BR`,
/// `zh-Hant-TW`.
pub fn check(tag: &str) -> Result<()> {
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    let subtags: Vec<&str> = parts.collect();
    let valid = (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_lowercase())
        && subtags.len() <= 3
        && subtags
            .iter()
            .all(|s| (2..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    ensure!(
        valid,
        "language {tag:?} is not a language tag such as fr, en or pt-BR"
    );
    Ok(())
}

/// The language a locale value names: `fr_FR.UTF-8@euro` names `fr-FR`. The
/// C and POSIX locales name none.
pub fn of_locale(value: &str) -> Option<String> {
    let name = value
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");
    if name.is_empty() || name == "C" || name == "POSIX" {
        return None;
    }
    check(&name).ok().map(|()| name)
}

/// The system's language, from the locale variables `var` reads, in the
/// order POSIX gives them precedence for messages: `LC_ALL`, `LC_MESSAGES`,
/// `LANG`. The first one set decides; without one naming a language,
/// English.
pub fn of_system(var: impl Fn(&str) -> Option<String>) -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| var(name).filter(|v| !v.is_empty()))
        .and_then(|value| of_locale(&value))
        .unwrap_or_else(|| DEFAULT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_language_tags_only() {
        for tag in ["fr", "en", "pt-BR", "zh-Hant-TW", "ast", "es-419"] {
            assert!(check(tag).is_ok(), "{tag}");
        }
        for tag in [
            "",
            "French",
            "FR",
            "fr_FR",
            "f",
            "fr-",
            "fr-F",
            "fr FR",
            "fr-a-b-c-d",
            "fr;rm",
        ] {
            assert!(check(tag).is_err(), "{tag:?}");
        }
    }

    #[test]
    fn reads_a_locale() {
        assert_eq!(of_locale("fr_FR.UTF-8").as_deref(), Some("fr-FR"));
        assert_eq!(of_locale("en_US.utf8").as_deref(), Some("en-US"));
        assert_eq!(of_locale("sr_RS@latin").as_deref(), Some("sr-RS"));
        assert_eq!(of_locale("de").as_deref(), Some("de"));
        for none in ["C", "C.UTF-8", "POSIX", "", ".UTF-8", "nonsense value"] {
            assert_eq!(of_locale(none), None, "{none:?}");
        }
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn the_locale_variables_take_precedence_as_posix_says() {
        assert_eq!(of_system(env(&[("LANG", "fr_FR.UTF-8")])), "fr-FR");
        assert_eq!(
            of_system(env(&[
                ("LANG", "fr_FR.UTF-8"),
                ("LC_MESSAGES", "de_DE.UTF-8")
            ])),
            "de-DE"
        );
        assert_eq!(
            of_system(env(&[
                ("LANG", "fr_FR.UTF-8"),
                ("LC_MESSAGES", "de_DE.UTF-8"),
                ("LC_ALL", "es_ES.UTF-8"),
            ])),
            "es-ES"
        );
        // An empty variable is unset; C names no language.
        assert_eq!(
            of_system(env(&[("LC_ALL", ""), ("LANG", "it_IT.UTF-8")])),
            "it-IT"
        );
        assert_eq!(of_system(env(&[("LANG", "C.UTF-8")])), "en");
        assert_eq!(of_system(env(&[])), "en");
    }
}
