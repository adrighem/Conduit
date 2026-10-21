//! Slack timestamp parsing and locale-aware message time text: the ISO
//! machine time, the full tooltip text and the compact relative label.
//! Calendar-day arithmetic lives in [`crate::day_label`].

use std::sync::OnceLock;

use gettextrs::gettext;

use crate::day_label::{capitalize_first_letter, date_text, days_old};

static TIME_FORMAT_LOCALE: OnceLock<Option<String>> = OnceLock::new();

fn normalize_language_tag(locale: &str) -> Option<String> {
    let locale = locale.trim();
    let (locale, modifier) = locale
        .split_once('@')
        .map_or((locale, None), |(locale, modifier)| {
            (locale, Some(modifier.to_ascii_lowercase()))
        });
    let locale = locale.split('.').next().unwrap_or_default();
    if locale.is_empty() || locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX")
    {
        return None;
    }

    let locale = locale.replace('_', "-");
    let subtags = locale.split('-').collect::<Vec<_>>();
    let language = subtags.first().copied()?;
    if !(2..=8).contains(&language.len())
        || !language
            .chars()
            .all(|character| character.is_ascii_alphabetic())
        || subtags.iter().any(|subtag| {
            subtag.is_empty()
                || subtag.len() > 8
                || !subtag
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
    {
        return None;
    }

    let mut normalized = subtags
        .iter()
        .enumerate()
        .map(|(index, subtag)| {
            if index == 0 {
                subtag.to_ascii_lowercase()
            } else if subtag.len() == 2
                && subtag
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
            {
                subtag.to_ascii_uppercase()
            } else if subtag.len() == 4
                && subtag
                    .chars()
                    .all(|character| character.is_ascii_alphabetic())
            {
                let mut characters = subtag.chars();
                characters
                    .next()
                    .map(|character| {
                        format!(
                            "{}{}",
                            character.to_ascii_uppercase(),
                            characters.as_str().to_ascii_lowercase()
                        )
                    })
                    .unwrap_or_default()
            } else {
                subtag.to_ascii_lowercase()
            }
        })
        .collect::<Vec<_>>();

    let modifier_script = match modifier.as_deref() {
        Some("arabic") => Some("Arab"),
        Some("cyrillic" | "cyrl") => Some("Cyrl"),
        Some("devanagari") => Some("Deva"),
        Some("hebrew") => Some("Hebr"),
        Some("ijekavianlatin" | "iqtelif" | "latin" | "latn") => Some("Latn"),
        Some("shaw") => Some("Shaw"),
        _ => None,
    };
    let modifier_variant = match modifier.as_deref() {
        Some("ije" | "ijekavian" | "ijekavianlatin") => Some("ijekavsk"),
        Some("valencia") => Some("valencia"),
        _ => None,
    };
    let has_script = normalized.iter().skip(1).any(|subtag| {
        subtag.len() == 4
            && subtag
                .chars()
                .all(|character| character.is_ascii_alphabetic())
    });
    if let Some(script) = modifier_script.filter(|_| !has_script) {
        normalized.insert(1, script.to_string());
    }
    if let Some(variant) = modifier_variant.filter(|variant| {
        !normalized
            .iter()
            .any(|subtag| subtag.eq_ignore_ascii_case(variant))
    }) {
        normalized.push(variant.to_string());
    }

    Some(normalized.join("-"))
}

pub(crate) fn initialize_time_format_locale(locale: Option<&[u8]>) {
    let _ = TIME_FORMAT_LOCALE.set(normalize_time_format_locale(locale));
}

fn normalize_time_format_locale(locale: Option<&[u8]>) -> Option<String> {
    locale
        .and_then(|locale| std::str::from_utf8(locale).ok())
        .and_then(normalize_language_tag)
}

#[cfg(test)]
fn preferred_time_locale<'a>(locales: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    for locale in locales.into_iter().flatten() {
        if !locale.trim().is_empty() {
            return normalize_language_tag(locale);
        }
    }
    None
}

pub(crate) fn localized_timestamp_parts(ts: &str) -> Option<(String, String, String)> {
    let datetime = slack_ts_datetime(ts)?;
    let now = gtk::glib::DateTime::now_local().ok()?;
    localized_timestamp_parts_at(&datetime, &now)
}

fn localized_timestamp_parts_at(
    datetime: &gtk::glib::DateTime,
    now: &gtk::glib::DateTime,
) -> Option<(String, String, String)> {
    let machine = datetime.format_iso8601().ok()?.to_string();
    let full = localized_full_timestamp(datetime)?;
    let short = compact_timestamp_text(datetime, now)?;
    Some((machine, full, short))
}

fn localized_full_timestamp(datetime: &gtk::glib::DateTime) -> Option<String> {
    let localized = datetime.format("%c").ok()?.to_string();
    let timezone = datetime.format("%Z").ok()?.to_string();
    Some(full_timestamp_with_timezone(&localized, &timezone))
}

fn full_timestamp_with_timezone(localized: &str, timezone: &str) -> String {
    let timezone = timezone.trim();
    let already_present = localized.split_whitespace().any(|part| {
        part.trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '+' && character != '-'
        })
        .eq_ignore_ascii_case(timezone)
    });
    if timezone.is_empty() || already_present {
        localized.to_string()
    } else {
        format!("{localized} {timezone}")
    }
}

fn compact_timestamp_text(
    datetime: &gtk::glib::DateTime,
    now: &gtk::glib::DateTime,
) -> Option<String> {
    let time = datetime.format(&gettext("%H:%M")).ok()?.to_string();
    let days_old = days_old(datetime, now);

    if days_old == 0 {
        return Some(time);
    }

    let day = if days_old == 1 {
        gettext("Yesterday")
    } else if (2..=6).contains(&days_old) {
        capitalize_first_letter(&datetime.format("%A").ok()?)
    } else {
        date_text(datetime, now)?
    };
    Some(
        gettext("{day}, {time}")
            .replace("{day}", &day)
            .replace("{time}", &time),
    )
}

pub(crate) fn slack_ts_datetime(ts: &str) -> Option<gtk::glib::DateTime> {
    let (seconds, _) = parse_slack_ts(ts)?;
    gtk::glib::DateTime::from_unix_local(seconds).ok()
}

fn parse_slack_ts(ts: &str) -> Option<(i64, u32)> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let seconds = seconds.parse::<i64>().ok()?;
    let mut fraction = fraction
        .chars()
        .take(9)
        .filter(|character| character.is_ascii_digit())
        .collect::<String>();
    while fraction.len() < 9 {
        fraction.push('0');
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()?
    };

    Some((seconds, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_posix_locales_to_escaped_bcp_47_language_tags() {
        assert_eq!(normalize_language_tag("nl_NL.UTF-8"), Some("nl-NL".into()));
        assert_eq!(
            normalize_language_tag("sr_RS@latin"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(
            normalize_language_tag("sr_RS@cyrillic"),
            Some("sr-Cyrl-RS".into())
        );
        assert_eq!(
            normalize_language_tag("ks_IN@devanagari"),
            Some("ks-Deva-IN".into())
        );
        assert_eq!(
            normalize_language_tag("tt_RU@iqtelif"),
            Some("tt-Latn-RU".into())
        );
        assert_eq!(
            normalize_language_tag("ca_ES@valencia"),
            Some("ca-ES-valencia".into())
        );
        assert_eq!(
            normalize_language_tag("sr_RS@ijekavianlatin"),
            Some("sr-Latn-RS-ijekavsk".into())
        );
        assert_eq!(
            normalize_language_tag("sr_Latn_RS@latin"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(
            normalize_language_tag("sr_Latn_RS@cyrillic"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(normalize_language_tag("C"), None);
        assert_eq!(normalize_language_tag("c.UTF-8"), None);
        assert_eq!(normalize_language_tag("POSIX"), None);
        assert_eq!(normalize_language_tag("posix.utf8"), None);
        assert_eq!(normalize_language_tag("en\"><script>"), None);
        assert_eq!(
            normalize_time_format_locale(Some(b"nl_NL.UTF-8")),
            Some("nl-NL".into())
        );
        assert_eq!(normalize_time_format_locale(Some(&[0xff, 0xfe])), None);
        assert_eq!(
            preferred_time_locale([None, Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
            Some("nl-NL".into())
        );
        assert_eq!(
            preferred_time_locale([Some(""), Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
            Some("nl-NL".into())
        );
        assert_eq!(
            preferred_time_locale([Some("C.UTF-8"), Some("nl_NL.UTF-8")]),
            None
        );
    }

    #[test]
    fn timestamp_tooltip_includes_the_timezone_once() {
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16 CEST", "CEST"),
            "do 02 jul 2026 10:42:16 CEST"
        );
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16", "CEST"),
            "do 02 jul 2026 10:42:16 CEST"
        );
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16", ""),
            "do 02 jul 2026 10:42:16"
        );
    }

    #[test]
    fn formats_timestamp_text_for_the_active_locale_and_keeps_iso_machine_time() {
        let (machine, full, short) = localized_timestamp_parts("1710000000.000100").unwrap();

        assert!(machine.contains('T'));
        assert!(!full.trim().is_empty());
        assert!(!short.trim().is_empty());
        assert!(localized_timestamp_parts("invalid").is_none());
    }

    #[test]
    fn compact_timestamp_uses_relative_days_then_dates_and_years() {
        let timezone = gtk::glib::TimeZone::local();
        let now = gtk::glib::DateTime::new(&timezone, 2026, 7, 15, 12, 0, 0.0).unwrap();
        let at = |year, month, day| {
            gtk::glib::DateTime::new(&timezone, year, month, day, 9, 30, 0.0).unwrap()
        };

        let today = at(2026, 7, 15);
        assert_eq!(
            compact_timestamp_text(&today, &now).as_deref(),
            today.format(&gettext("%H:%M")).ok().as_deref()
        );

        let yesterday = at(2026, 7, 14);
        assert!(compact_timestamp_text(&yesterday, &now)
            .unwrap()
            .starts_with("Yesterday, "));

        let five_days_ago = at(2026, 7, 10);
        let weekday = capitalize_first_letter(&five_days_ago.format("%A").unwrap());
        assert!(compact_timestamp_text(&five_days_ago, &now)
            .unwrap()
            .contains(&weekday));

        let six_days_ago = at(2026, 7, 9);
        let weekday_six = capitalize_first_letter(&six_days_ago.format("%A").unwrap());
        assert!(compact_timestamp_text(&six_days_ago, &now)
            .unwrap()
            .contains(&weekday_six));

        let seven_days_ago = at(2026, 7, 8);
        let seven_days_label = compact_timestamp_text(&seven_days_ago, &now).unwrap();
        assert!(seven_days_label.contains("8 Jul"));
        assert!(!seven_days_label.contains("2026"));

        let recent_previous_year_now = at(2026, 1, 10);
        let recent_previous_year = at(2025, 12, 31);
        assert!(
            compact_timestamp_text(&recent_previous_year, &recent_previous_year_now)
                .unwrap()
                .contains("31 Dec 2025")
        );

        let old_previous_year = at(2025, 12, 31);
        assert!(compact_timestamp_text(&old_previous_year, &now)
            .unwrap()
            .contains("2025"));
    }
}
