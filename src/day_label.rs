//! Pure local-calendar-day helpers shared by message timestamps and the
//! timeline day separators. Everything here takes explicit `(datetime, now)`
//! inputs so it is deterministic under test.

use gettextrs::gettext;
use gtk::glib::DateTime;

/// Uppercases the first character (locale month/weekday names may be lower case).
pub(crate) fn capitalize_first_letter(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Monotonic day number of the civil (local) date. Time and UTC offset are
/// intentionally ignored: relative labels follow the user's local dates.
pub(crate) fn local_calendar_day(datetime: &DateTime) -> i64 {
    let mut year = i64::from(datetime.year());
    let month = i64::from(datetime.month());
    let day = i64::from(datetime.day_of_month());
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year
}

/// Whole calendar days between `datetime` and `now` (1 means yesterday).
pub(crate) fn days_old(datetime: &DateTime, now: &DateTime) -> i64 {
    local_calendar_day(now) - local_calendar_day(datetime)
}

/// Absolute date text: "%e %b" within the current year, "%e %b %Y" otherwise.
pub(crate) fn date_text(datetime: &DateTime, now: &DateTime) -> Option<String> {
    let include_year = days_old(datetime, now) >= 183 || datetime.year() != now.year();
    let format_str = if include_year {
        gettext("%e %b %Y")
    } else {
        gettext("%e %b")
    };
    let raw = datetime
        .format(&format_str)
        .ok()?
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(capitalize_first_letter(&raw))
}

/// Day-only label: "Today", "Yesterday", "15 Sep" or "15 Sep 2025".
pub(crate) fn day_label(datetime: &DateTime, now: &DateTime) -> Option<String> {
    match days_old(datetime, now) {
        0 => Some(gettext("Today")),
        1 => Some(gettext("Yesterday")),
        _ => date_text(datetime, now),
    }
}

/// Whether a separator belongs before item `index`, given each item's local
/// calendar day (`None` when its timestamp is unparsable). The first dated
/// item always gets one so the oldest loaded history is dated too.
pub(crate) fn separator_before(days: &[Option<i64>], index: usize) -> bool {
    let Some(Some(current)) = days.get(index) else {
        return false;
    };
    let previous = days[..index].iter().rev().find_map(|day| *day);
    previous != Some(*current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> gtk::glib::TimeZone {
        gtk::glib::TimeZone::local()
    }

    fn at(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> DateTime {
        DateTime::new(&tz(), year, month, day, hour, minute, 0.0).unwrap()
    }

    #[test]
    fn today_and_yesterday_use_names() {
        let now = at(2026, 7, 15, 12, 0);
        assert_eq!(day_label(&at(2026, 7, 15, 0, 1), &now).unwrap(), "Today");
        assert_eq!(day_label(&at(2026, 7, 14, 23, 59), &now).unwrap(), "Yesterday");
    }

    #[test]
    fn yesterday_is_by_calendar_date_across_midnight() {
        let now = at(2026, 7, 15, 0, 5);
        let late = at(2026, 7, 14, 23, 55);
        assert_eq!(days_old(&late, &now), 1);
        assert_eq!(day_label(&late, &now).unwrap(), "Yesterday");
        let now_late = at(2026, 7, 15, 23, 59);
        assert_eq!(day_label(&at(2026, 7, 14, 0, 0), &now_late).unwrap(), "Yesterday");
    }

    #[test]
    fn same_year_omits_year() {
        let now = at(2026, 7, 15, 12, 0);
        assert_eq!(day_label(&at(2026, 3, 5, 9, 0), &now).unwrap(), "5 Mar");
    }

    #[test]
    fn other_year_includes_year() {
        let now = at(2026, 7, 15, 12, 0);
        assert_eq!(day_label(&at(2025, 9, 15, 9, 0), &now).unwrap(), "15 Sep 2025");
    }

    #[test]
    fn year_boundary_dec_31_and_jan_1() {
        let now = at(2026, 1, 1, 10, 0);
        assert_eq!(day_label(&at(2025, 12, 31, 23, 0), &now).unwrap(), "Yesterday");
        let later = at(2026, 1, 2, 10, 0);
        assert_eq!(day_label(&at(2025, 12, 31, 23, 0), &later).unwrap(), "31 Dec 2025");
        assert_eq!(day_label(&at(2026, 1, 1, 0, 0), &later).unwrap(), "Yesterday");
    }

    #[test]
    fn separator_before_marks_day_changes() {
        let days = [Some(10), Some(10), Some(11), Some(11), Some(13)];
        let flags: Vec<bool> = (0..days.len()).map(|i| separator_before(&days, i)).collect();
        assert_eq!(flags, [true, false, true, false, true]);
    }

    #[test]
    fn separator_before_skips_undated_items() {
        let days = [Some(10), None, Some(10), None, Some(11)];
        let flags: Vec<bool> = (0..days.len()).map(|i| separator_before(&days, i)).collect();
        assert_eq!(flags, [true, false, false, false, true]);
        assert!(!separator_before(&days, 99));
    }
}
