use std::collections::HashSet;

use gtk::gio;
use gtk::prelude::*;

use crate::attention::{normalize_configured_term, AttentionPreferences};
use crate::config;

pub(crate) const ATTENTION_SETTINGS_KEYS: [&str; 6] = [
    config::NOTIFICATIONS_ENABLED_V1_KEY,
    config::NOTIFICATIONS_DIRECT_MESSAGES_V1_KEY,
    config::NOTIFICATIONS_MENTIONS_AND_NAMES_V1_KEY,
    config::NOTIFICATIONS_THREAD_REPLIES_V1_KEY,
    config::NOTIFICATIONS_NAMES_AND_ALIASES_V1_KEY,
    config::NOTIFICATIONS_KEYWORDS_V1_KEY,
];

const MAX_TERMS: usize = 64;
const MAX_TERM_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TermListError {
    TooMany,
    TooLong,
    MissingWord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TermListKind {
    Aliases,
    Keywords,
}

impl TermListError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::TooMany => "Use no more than 64 entries.",
            Self::TooLong => "Keep each entry to 128 characters or fewer.",
            Self::MissingWord => "Each entry needs at least one letter or number.",
        }
    }
}

pub(crate) fn load(settings: &gio::Settings) -> AttentionPreferences {
    AttentionPreferences {
        desktop_notifications: settings.boolean(config::NOTIFICATIONS_ENABLED_V1_KEY),
        direct_messages: settings.boolean(config::NOTIFICATIONS_DIRECT_MESSAGES_V1_KEY),
        mentions_and_names: settings.boolean(config::NOTIFICATIONS_MENTIONS_AND_NAMES_V1_KEY),
        thread_replies: settings.boolean(config::NOTIFICATIONS_THREAD_REPLIES_V1_KEY),
        names_and_aliases: stored_terms(
            settings.strv(config::NOTIFICATIONS_NAMES_AND_ALIASES_V1_KEY),
            TermListKind::Aliases,
        ),
        keywords: stored_terms(
            settings.strv(config::NOTIFICATIONS_KEYWORDS_V1_KEY),
            TermListKind::Keywords,
        ),
    }
}

pub(crate) fn is_attention_setting(key: &str) -> bool {
    ATTENTION_SETTINGS_KEYS.contains(&key)
}

pub(crate) fn parse_term_lines(
    text: &str,
    kind: TermListKind,
) -> Result<Vec<String>, TermListError> {
    let candidates = text.lines().map(str::trim).filter(|term| !term.is_empty());
    let mut terms = Vec::new();
    let mut normalized = HashSet::new();
    for term in candidates {
        let (term, identity) = prepare_term(term, kind)?;
        if normalized.insert(identity) {
            terms.push(term);
        }
        if terms.len() > MAX_TERMS {
            return Err(TermListError::TooMany);
        }
    }
    Ok(terms)
}

pub(crate) fn format_term_lines(terms: &[String]) -> String {
    terms.join("\n")
}

fn stored_terms<I, S>(terms: I, kind: TermListKind) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut stored = Vec::new();
    let mut normalized = HashSet::new();
    for term in terms {
        let Ok((term, identity)) = prepare_term(term.as_ref(), kind) else {
            continue;
        };
        if normalized.insert(identity) {
            stored.push(term);
        }
        if stored.len() >= MAX_TERMS {
            break;
        }
    }
    stored
}

fn prepare_term(term: &str, kind: TermListKind) -> Result<(String, String), TermListError> {
    let term = term.split_whitespace().collect::<Vec<_>>().join(" ");
    let term = if kind == TermListKind::Aliases {
        term.strip_prefix('@')
            .unwrap_or(&term)
            .trim_start()
            .to_string()
    } else {
        term
    };
    if term.chars().count() > MAX_TERM_CHARS {
        return Err(TermListError::TooLong);
    }
    let identity = normalize_configured_term(&term);
    if !identity.chars().any(char::is_alphanumeric) {
        return Err(TermListError::MissingWord);
    }
    Ok((term, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_keys_are_explicitly_versioned() {
        assert!(ATTENTION_SETTINGS_KEYS
            .iter()
            .all(|key| key.ends_with("-v1")));
        assert_eq!(
            ATTENTION_SETTINGS_KEYS.iter().collect::<HashSet<_>>().len(),
            ATTENTION_SETTINGS_KEYS.len()
        );
    }

    #[test]
    fn term_lines_trim_deduplicate_and_preserve_phrases_and_punctuation() {
        let text = "  Vincent  \nincident review, today\nINCIDENT REVIEW, TODAY\nnaïve café\n";
        let terms = parse_term_lines(text, TermListKind::Keywords).unwrap();

        assert_eq!(terms, ["Vincent", "incident review, today", "naïve café"]);
        assert_eq!(format_term_lines(&terms), terms.join("\n"));
    }

    #[test]
    fn empty_term_list_is_valid_but_invalid_entries_are_rejected() {
        assert_eq!(
            parse_term_lines(" \n\n", TermListKind::Keywords).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_term_lines("---", TermListKind::Keywords),
            Err(TermListError::MissingWord)
        );
        assert_eq!(
            parse_term_lines(&"x".repeat(MAX_TERM_CHARS + 1), TermListKind::Keywords),
            Err(TermListError::TooLong)
        );
        assert_eq!(
            parse_term_lines(
                &(0..=MAX_TERMS)
                    .map(|index| format!("term {index}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                TermListKind::Keywords,
            ),
            Err(TermListError::TooMany)
        );
    }

    #[test]
    fn aliases_strip_at_collapse_whitespace_and_use_canonical_deduplication() {
        assert_eq!(
            parse_term_lines(
                " @Vincent van Adrighem\nvincent   van   adrighem\nVíncent",
                TermListKind::Aliases,
            )
            .unwrap(),
            ["Vincent van Adrighem", "Víncent"]
        );
    }

    #[test]
    fn unrelated_settings_do_not_trigger_attention_updates() {
        assert!(is_attention_setting(
            config::NOTIFICATIONS_DIRECT_MESSAGES_V1_KEY
        ));
        assert!(!is_attention_setting(config::WINDOW_WIDTH_KEY));
    }

    #[test]
    fn stored_terms_fail_closed_for_invalid_duplicate_and_excess_values() {
        let mut values = vec![
            "---".to_string(),
            " @Vincent ".to_string(),
            "víncent".to_string(),
        ];
        values.extend((0..MAX_TERMS).map(|index| format!("term {index}")));

        let stored = stored_terms(values, TermListKind::Aliases);

        assert_eq!(stored.first().map(String::as_str), Some("Vincent"));
        assert_eq!(stored.len(), MAX_TERMS);
        assert_eq!(stored.last().map(String::as_str), Some("term 62"));
    }
}
