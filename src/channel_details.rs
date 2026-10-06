//! Pure view-model helpers for the native channel details dialog.
//!
//! Everything here is widget free so the behaviour (what to show for which
//! conversation kind, text conversion, member search) is unit tested.

use std::collections::HashMap;

use gettextrs::gettext;
use serde_json::Value;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::models::SlackConversation;
use crate::sidebar::ConversationKind;

/// What the conversation title does when clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TitleBehavior {
    /// One-to-one DMs show the other person's profile.
    OpenProfile,
    /// Channels and group DMs open the details dialog.
    OpenDetails(DetailsLayout),
}

/// Which tabs the details dialog offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DetailsLayout {
    pub(crate) about: bool,
    pub(crate) settings: bool,
}

pub(crate) fn title_behavior(kind: ConversationKind) -> Option<TitleBehavior> {
    match kind {
        ConversationKind::DirectMessage => Some(TitleBehavior::OpenProfile),
        ConversationKind::GroupDirectMessage => Some(TitleBehavior::OpenDetails(DetailsLayout {
            about: false,
            settings: false,
        })),
        ConversationKind::PublicChannel | ConversationKind::PrivateChannel => {
            Some(TitleBehavior::OpenDetails(DetailsLayout {
                about: true,
                settings: true,
            }))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AboutModel {
    pub(crate) topic: String,
    pub(crate) purpose: String,
    pub(crate) creator_id: Option<String>,
    pub(crate) created: Option<i64>,
    pub(crate) member_count: Option<usize>,
}

fn text_field(conversation: &SlackConversation, key: &str) -> String {
    conversation
        .extra
        .get(key)
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(crate) fn about_model(conversation: &SlackConversation) -> AboutModel {
    let creator_id = conversation
        .extra
        .get("creator")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let created = conversation.extra.get("created").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
    });
    let member_count = conversation
        .extra
        .get("num_members")
        .and_then(Value::as_u64)
        .map(|count| count as usize);
    AboutModel {
        topic: text_field(conversation, "topic"),
        purpose: text_field(conversation, "purpose"),
        creator_id,
        created: created.filter(|ts| *ts > 0),
        member_count,
    }
}

/// Days since 1970-01-01 to (year, month 1-12, day 1-31), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// "6 Oct 2026" for a unix timestamp, shifted by `offset_secs` to local time.
pub(crate) fn format_date(unix_secs: i64, offset_secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, day) = civil_from_days((unix_secs + offset_secs).div_euclid(86_400));
    format!("{day} {} {year}", gettext(MONTHS[month as usize - 1]))
}

/// "Ada Lovelace on 6 Oct 2026"; degrades to whichever half is known.
pub(crate) fn created_by_text(
    creator: Option<&str>,
    created: Option<i64>,
    offset_secs: i64,
) -> Option<String> {
    let creator = creator.map(str::trim).filter(|name| !name.is_empty());
    match (creator, created) {
        (Some(name), Some(ts)) => Some(
            gettext("{name} on {date}")
                .replace("{name}", name)
                .replace("{date}", &format_date(ts, offset_secs)),
        ),
        (Some(name), None) => Some(name.to_string()),
        (None, Some(ts)) => {
            Some(gettext("On {date}").replace("{date}", &format_date(ts, offset_secs)))
        }
        (None, None) => None,
    }
}

/// Tab title such as "Members (12)"; no count until one is known.
pub(crate) fn members_tab_title(count: Option<usize>) -> String {
    match count {
        Some(count) => format!("{} ({count})", gettext("Members")),
        None => gettext("Members"),
    }
}

/// Converts Slack mrkdwn text (topic, purpose) to plain text: unicode emoji,
/// readable mentions and links, no markup.
pub(crate) fn slack_text_to_plain(
    text: &str,
    custom_emojis: &HashMap<String, String>,
    user_name: &dyn Fn(&str) -> Option<String>,
) -> String {
    let catalog = EmojiCatalog::new(custom_emojis);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        if ch == '<' {
            if let Some(end) = rest.find('>') {
                out.push_str(&plain_token(&rest[1..end], user_name));
                rest = &rest[end + 1..];
                continue;
            }
        } else if ch == ':' {
            if let Some((glyph, len)) = emoji_at(rest, &catalog) {
                out.push_str(glyph);
                rest = &rest[len..];
                continue;
            }
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn emoji_at(text: &str, catalog: &EmojiCatalog<'_>) -> Option<(&'static str, usize)> {
    let end = text[1..].find(':')? + 1;
    let name = &text[1..end];
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '\''));
    if !valid {
        return None;
    }
    match catalog.resolve(name)? {
        EmojiValue::Unicode(glyph) => Some((glyph, end + 1)),
        EmojiValue::CustomImage(_) => None,
    }
}

fn plain_token(token: &str, user_name: &dyn Fn(&str) -> Option<String>) -> String {
    let (target, label) = match token.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (token, None),
    };
    if let Some(user_id) = target.strip_prefix('@') {
        return match label.filter(|l| !l.is_empty()) {
            Some(label) => format!("@{label}"),
            None => format!(
                "@{}",
                user_name(user_id).unwrap_or_else(|| user_id.to_string())
            ),
        };
    }
    if target.starts_with('#') {
        return match label.filter(|l| !l.is_empty()) {
            Some(label) => format!("#{label}"),
            None => target.to_string(),
        };
    }
    if let Some(command) = target.strip_prefix('!') {
        return format!("@{}", label.unwrap_or(command));
    }
    label
        .filter(|l| !l.is_empty())
        .unwrap_or(target)
        .to_string()
}

/// One row of the members list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberRow {
    pub(crate) user_id: String,
    pub(crate) name: String,
    /// Status as "emoji text", empty when none.
    pub(crate) status: String,
    pub(crate) avatar_path: Option<std::path::PathBuf>,
}

/// Case-insensitive match on name, status text or user ID.
pub(crate) fn member_matches(row: &MemberRow, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || row.name.to_lowercase().contains(&query)
        || row.status.to_lowercase().contains(&query)
        || row.user_id.to_lowercase().contains(&query)
}

pub(crate) fn filter_members<'a>(rows: &'a [MemberRow], query: &str) -> Vec<&'a MemberRow> {
    rows.iter()
        .filter(|row| member_matches(row, query))
        .collect()
}

/// Sorts by case-insensitive name, then ID, dropping duplicate IDs.
pub(crate) fn sort_members(rows: &mut Vec<MemberRow>) {
    rows.sort_by_key(|row| (row.name.to_lowercase(), row.user_id.clone()));
    rows.dedup_by(|a, b| a.user_id == b.user_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversation(extra: Value) -> SlackConversation {
        serde_json::from_value(json!({"id": "C1", "name": "general", "is_channel": true}))
            .map(|mut c: SlackConversation| {
                if let Value::Object(map) = extra {
                    c.extra.extend(map);
                }
                c
            })
            .unwrap()
    }

    fn member(id: &str, name: &str, status: &str) -> MemberRow {
        MemberRow {
            user_id: id.into(),
            name: name.into(),
            status: status.into(),
            avatar_path: None,
        }
    }

    #[test]
    fn formats_dates_with_offset() {
        assert_eq!(format_date(0, 0), "1 Jan 1970");
        assert_eq!(format_date(1_791_244_800, 0), "6 Oct 2026");
        assert_eq!(format_date(1_791_244_800 - 1, 0), "5 Oct 2026");
        assert_eq!(format_date(1_791_244_800 - 1, 3600), "6 Oct 2026");
        assert_eq!(format_date(951_782_400, 0), "29 Feb 2000");
    }

    #[test]
    fn created_by_text_degrades_gracefully() {
        let ts = Some(1_791_244_800);
        assert_eq!(
            created_by_text(Some(" Ada "), ts, 0).as_deref(),
            Some("Ada on 6 Oct 2026")
        );
        assert_eq!(
            created_by_text(Some("Ada"), None, 0).as_deref(),
            Some("Ada")
        );
        assert_eq!(
            created_by_text(None, ts, 0).as_deref(),
            Some("On 6 Oct 2026")
        );
        assert_eq!(created_by_text(Some("  "), None, 0), None);
    }

    #[test]
    fn about_model_reads_conversation_info() {
        let model = about_model(&conversation(json!({
            "topic": {"value": " Ship it ", "creator": "U1", "last_set": 1},
            "purpose": {"value": "Release chat"},
            "creator": "U2",
            "created": 1_791_244_800,
            "num_members": 7
        })));
        assert_eq!(model.topic, "Ship it");
        assert_eq!(model.purpose, "Release chat");
        assert_eq!(model.creator_id.as_deref(), Some("U2"));
        assert_eq!(model.created, Some(1_791_244_800));
        assert_eq!(model.member_count, Some(7));
        assert_eq!(about_model(&conversation(json!({}))), AboutModel::default());
    }

    #[test]
    fn tab_title_includes_count_when_known() {
        assert_eq!(members_tab_title(Some(12)), "Members (12)");
        assert_eq!(members_tab_title(Some(0)), "Members (0)");
        assert_eq!(members_tab_title(None), "Members");
    }

    #[test]
    fn plain_text_converts_emoji_mentions_links_and_entities() {
        let names = |id: &str| (id == "U1").then(|| "Ada".to_string());
        let custom = HashMap::new();
        let plain = |text: &str| slack_text_to_plain(text, &custom, &names);
        assert_eq!(plain("Ship :tada: now"), "Ship \u{1f389} now");
        assert_eq!(plain("ping <@U1> and <@U9>"), "ping @Ada and @U9");
        assert_eq!(plain("see <#C1|general>"), "see #general");
        assert_eq!(
            plain("<https://x.io|docs> <https://y.io>"),
            "docs https://y.io"
        );
        assert_eq!(plain("<!here> a &amp; b &lt;c&gt;"), "@here a & b <c>");
        assert_eq!(
            plain("time 10:30:45 :nope_x: :"),
            "time 10:30:45 :nope_x: :"
        );
        assert_eq!(plain("<broken"), "<broken");
    }

    #[test]
    fn member_search_matches_name_status_and_id() {
        let rows = vec![
            member("U1", "Ada Lovelace", ""),
            member("U2", "Grace Hopper", "On vacation"),
        ];
        assert_eq!(filter_members(&rows, "").len(), 2);
        assert_eq!(filter_members(&rows, " ADA ")[0].user_id, "U1");
        assert_eq!(filter_members(&rows, "vacation")[0].user_id, "U2");
        assert_eq!(filter_members(&rows, "u2")[0].name, "Grace Hopper");
        assert!(filter_members(&rows, "zzz").is_empty());
    }

    #[test]
    fn sorting_is_case_insensitive_and_dedups() {
        let mut rows = vec![
            member("U2", "bob", ""),
            member("U1", "Alice", ""),
            member("U2", "bob", ""),
        ];
        sort_members(&mut rows);
        let ids: Vec<_> = rows.iter().map(|r| r.user_id.as_str()).collect();
        assert_eq!(ids, ["U1", "U2"]);
    }

    #[test]
    fn title_behavior_depends_on_conversation_kind() {
        assert_eq!(
            title_behavior(ConversationKind::DirectMessage),
            Some(TitleBehavior::OpenProfile)
        );
        let Some(TitleBehavior::OpenDetails(group)) =
            title_behavior(ConversationKind::GroupDirectMessage)
        else {
            panic!("group DM opens details");
        };
        assert!(!group.about && !group.settings);
        for kind in [
            ConversationKind::PublicChannel,
            ConversationKind::PrivateChannel,
        ] {
            let Some(TitleBehavior::OpenDetails(layout)) = title_behavior(kind) else {
                panic!("channels open details");
            };
            assert!(layout.about && layout.settings);
        }
        assert_eq!(title_behavior(ConversationKind::Unknown), None);
    }
}
