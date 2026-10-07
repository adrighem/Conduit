//! Pure profile view model: formatting of names, status, local time and rows.

use std::collections::HashMap;
use std::path::PathBuf;

use gettextrs::gettext;
use gtk::glib;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::models::SlackUser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Email,
    Phone,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailRow {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusLine {
    pub(crate) text: String,
    pub(crate) expiry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileViewModel {
    pub(crate) display_name: String,
    /// Real name and pronouns joined for the header, when present.
    pub(crate) identity_line: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) status: Option<StatusLine>,
    pub(crate) local_time: Option<String>,
    pub(crate) rows: Vec<DetailRow>,
}

/// Everything the window knows about the profile being shown.
pub(crate) struct ProfileInput {
    pub(crate) user: SlackUser,
    pub(crate) avatar_path: Option<PathBuf>,
    pub(crate) custom_emojis: HashMap<String, String>,
    pub(crate) can_message: bool,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `HH:MM` of a unix timestamp shifted by `offset_secs`.
fn clock_text(unix_secs: i64, offset_secs: i64) -> String {
    let day_secs = (unix_secs + offset_secs).rem_euclid(86_400);
    format!("{:02}:{:02}", day_secs / 3600, day_secs % 3600 / 60)
}

/// "14:05 local time" for the user's UTC offset, if Slack provided one.
pub(crate) fn local_time_text(tz_offset: Option<i64>, now: i64) -> Option<String> {
    let offset = tz_offset?;
    Some(gettext("{time} local time").replace("{time}", &clock_text(now, offset)))
}

/// "Until 15:30" for a status ending later today (in the viewer's zone), or
/// "Until <date> 15:30" for a later day. `None` when it never expires or has
/// already expired.
pub(crate) fn status_expiry_text(expiration: i64, now: i64, viewer_offset: i64) -> Option<String> {
    if expiration <= 0 || expiration <= now {
        return None;
    }
    let same_day =
        (expiration + viewer_offset).div_euclid(86_400) == (now + viewer_offset).div_euclid(86_400);
    let clock = clock_text(expiration, viewer_offset);
    let when = if same_day {
        clock
    } else {
        let date = glib::DateTime::from_unix_utc(expiration + viewer_offset)
            .ok()
            .and_then(|date| date.format("%x").ok())
            .map(|date| date.to_string())
            .unwrap_or_default();
        format!("{date} {clock}")
    };
    Some(gettext("Until {when}").replace("{when}", &when))
}

pub(crate) fn profile_view_model(
    user: &SlackUser,
    custom_emojis: &HashMap<String, String>,
    now: i64,
    viewer_offset: i64,
) -> ProfileViewModel {
    let profile = user.profile.as_ref();
    let display_name = user
        .display_name()
        .unwrap_or_else(|| gettext("Unknown person"));
    let real_name = user
        .full_name()
        .filter(|name| !name.eq_ignore_ascii_case(&display_name));
    let pronouns = non_empty(profile.and_then(|p| p.pronouns.as_deref()));
    let identity_line = match (real_name, pronouns) {
        (Some(name), Some(pronouns)) => Some(format!("{name} · {pronouns}")),
        (name, pronouns) => name.or(pronouns),
    };

    let status = user
        .status()
        .filter(|status| status.active_at(now))
        .map(|status| {
            let emoji = if status.emoji_name().is_empty() {
                String::new()
            } else {
                match EmojiCatalog::new(custom_emojis).resolve(status.emoji_name()) {
                    Some(EmojiValue::Unicode(glyph)) => glyph.to_string(),
                    _ => String::new(),
                }
            };
            let text = [emoji.as_str(), status.text.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            StatusLine {
                text: if text.is_empty() {
                    status.accessible_text()
                } else {
                    text
                },
                expiry: status_expiry_text(status.expiration, now, viewer_offset),
            }
        });

    let mut rows = Vec::new();
    let mut push = |label: String, value: Option<String>, kind: RowKind| {
        if let Some(value) = value {
            rows.push(DetailRow { label, value, kind });
        }
    };
    push(
        gettext("Email"),
        non_empty(profile.and_then(|p| p.email.as_deref())),
        RowKind::Email,
    );
    push(
        gettext("Phone"),
        non_empty(profile.and_then(|p| p.phone.as_deref())),
        RowKind::Phone,
    );
    push(
        gettext("Skype"),
        non_empty(profile.and_then(|p| p.skype.as_deref())),
        RowKind::Text,
    );
    push(
        gettext("Location"),
        non_empty(
            profile
                .and_then(|p| p.location.as_deref())
                .or(user.tz_label.as_deref()),
        ),
        RowKind::Text,
    );
    push(
        gettext("About"),
        non_empty(profile.and_then(|p| p.about.as_deref())),
        RowKind::Text,
    );
    if let Some(profile) = profile {
        let mut fields = profile.fields.iter().collect::<Vec<_>>();
        fields.sort_by_key(|(id, field)| field.label.as_deref().unwrap_or(id).to_lowercase());
        for (id, field) in fields {
            push(
                field.label.clone().unwrap_or_else(|| id.clone()),
                non_empty(field.display_value()),
                RowKind::Text,
            );
        }
    }

    ProfileViewModel {
        display_name,
        identity_line,
        title: non_empty(profile.and_then(|p| p.title.as_deref())),
        status,
        local_time: local_time_text(user.tz_offset, now),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SlackProfileField, SlackUserProfile};

    const NOW: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC

    fn user(profile: SlackUserProfile) -> SlackUser {
        SlackUser {
            id: Some("U1".into()),
            real_name: Some("Alice Doe".into()),
            profile: Some(profile),
            ..Default::default()
        }
    }

    #[test]
    fn local_time_uses_tz_offset() {
        assert_eq!(
            local_time_text(Some(3600), NOW).as_deref(),
            Some("23:13 local time")
        );
        assert_eq!(
            local_time_text(Some(-8 * 3600), NOW).as_deref(),
            Some("14:13 local time")
        );
        assert_eq!(
            local_time_text(Some(2 * 3600), NOW).as_deref(),
            Some("00:13 local time")
        );
        assert_eq!(local_time_text(None, NOW), None);
    }

    #[test]
    fn status_expiry_is_hidden_when_unset_or_past() {
        assert_eq!(status_expiry_text(0, NOW, 0), None);
        assert_eq!(status_expiry_text(NOW - 1, NOW, 0), None);
    }

    #[test]
    fn status_expiry_shows_clock_today_and_date_later() {
        assert_eq!(
            status_expiry_text(NOW + 1800, NOW, 0).as_deref(),
            Some("Until 22:43")
        );
        let later = status_expiry_text(NOW + 86_400, NOW, 0).unwrap();
        assert!(
            later.starts_with("Until ") && later.ends_with(" 22:13"),
            "{later}"
        );
        // 22:43 UTC is already the next day for a viewer at UTC+2.
        let shifted = status_expiry_text(NOW + 1800, NOW, 2 * 3600).unwrap();
        assert!(shifted.ends_with(" 00:43"), "{shifted}");
    }

    #[test]
    fn missing_fields_are_hidden() {
        let model = profile_view_model(&user(SlackUserProfile::default()), &HashMap::new(), NOW, 0);
        assert_eq!(model.display_name, "Alice Doe");
        assert_eq!(model.identity_line, None);
        assert_eq!(model.title, None);
        assert_eq!(model.status, None);
        assert_eq!(model.local_time, None);
        assert!(model.rows.is_empty());
    }

    #[test]
    fn populated_profile_formats_header_and_rows() {
        let mut profile = SlackUserProfile {
            display_name: Some("ali".into()),
            title: Some(" Engineer ".into()),
            pronouns: Some("she/her".into()),
            email: Some("a@example.com".into()),
            phone: Some("  ".into()),
            status_text: Some("Out sailing".into()),
            status_emoji: Some(":sailboat:".into()),
            status_expiration: Some(NOW + 1800),
            ..Default::default()
        };
        profile.fields.insert(
            "Xf2".into(),
            SlackProfileField {
                value: Some("v".into()),
                alt: Some("Team B".into()),
                label: Some("Squad".into()),
            },
        );
        let mut user = user(profile);
        user.tz_offset = Some(3600);
        user.tz_label = Some("Central European Time".into());
        let model = profile_view_model(&user, &HashMap::new(), NOW, 0);
        assert_eq!(model.display_name, "ali");
        assert_eq!(model.identity_line.as_deref(), Some("Alice Doe · she/her"));
        assert_eq!(model.title.as_deref(), Some("Engineer"));
        let status = model.status.unwrap();
        assert!(status.text.ends_with("Out sailing"));
        assert_eq!(status.expiry.as_deref(), Some("Until 22:43"));
        assert_eq!(model.local_time.as_deref(), Some("23:13 local time"));
        let rows: Vec<_> = model
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Email", "a@example.com"),
                ("Location", "Central European Time"),
                ("Squad", "Team B"),
            ]
        );
        assert_eq!(model.rows[0].kind, RowKind::Email);
    }

    #[test]
    fn expired_status_is_omitted() {
        let profile = SlackUserProfile {
            status_text: Some("Lunch".into()),
            status_expiration: Some(NOW - 10),
            ..Default::default()
        };
        assert_eq!(
            profile_view_model(&user(profile), &HashMap::new(), NOW, 0).status,
            None
        );
    }
}
