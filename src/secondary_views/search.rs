//! Search result rows with query highlighting.

use gettextrs::gettext;

use std::collections::HashMap;

use super::model::{is_http_url, plain_text, DirectoryRow, DirectoryTarget, SearchRow};
use crate::message_markup::MessageRenderContext;
use crate::models::SearchMatch;
use crate::search::{
    normalize_for_search, SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT,
    SECONDARY_FIELD_WEIGHT,
};
use crate::sidebar::{ConversationKind, UserSearchAliases};

/// What the search bar looks through. Messages and direct messages are
/// searched by Slack; people and channels by name in the local directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SearchScope {
    #[default]
    Messages,
    DirectMessages,
    People,
    Channels,
}

impl SearchScope {
    /// Matches the order of the "Type" dropdown.
    pub(crate) fn from_index(index: u32) -> Self {
        match index {
            1 => Self::DirectMessages,
            2 => Self::People,
            3 => Self::Channels,
            _ => Self::Messages,
        }
    }

    /// Answered from the local directory, without asking Slack.
    pub(crate) fn is_local(self) -> bool {
        matches!(self, Self::People | Self::Channels)
    }
}

/// Most people or channels listed for one query.
const MAX_DIRECTORY_ROWS: usize = 100;

/// People whose name, full name, alias or id matches `query`, best first.
/// An empty query lists everyone alphabetically.
pub(crate) fn people_rows(
    user_names: &HashMap<String, String>,
    user_full_names: &HashMap<String, String>,
    aliases: &UserSearchAliases,
    query: &str,
) -> Vec<DirectoryRow> {
    let query = SearchQuery::parse(query);
    let mut ranked = user_names
        .iter()
        .filter_map(|(id, name)| {
            let full_name = user_full_names
                .get(id)
                .map(String::as_str)
                .unwrap_or_default();
            let score = query.score(
                [
                    SearchField::new(name, PRIMARY_FIELD_WEIGHT),
                    SearchField::new(full_name, SECONDARY_FIELD_WEIGHT),
                    SearchField::new(id, ID_FIELD_WEIGHT),
                ]
                .into_iter()
                .chain(
                    aliases
                        .get(id)
                        .into_iter()
                        .flatten()
                        .map(|alias| SearchField::new(alias, SECONDARY_FIELD_WEIGHT)),
                ),
            )?;
            let detail = if full_name.is_empty() || full_name == name {
                gettext("Direct message")
            } else {
                full_name.to_string()
            };
            Some((
                score.band(),
                crate::search::normalize_for_search(name),
                DirectoryRow {
                    title: name.clone(),
                    detail,
                    target: DirectoryTarget::Person(id.clone()),
                    icon_name: ConversationKind::DirectMessage.icon_name(),
                },
            ))
        })
        .collect::<Vec<_>>();
    rank_directory_rows(&mut ranked)
}

/// Public and private channels whose name or id matches `query`, best first.
pub(crate) fn channel_rows(
    channels: impl IntoIterator<Item = (String, String, ConversationKind)>,
    query: &str,
) -> Vec<DirectoryRow> {
    let query = SearchQuery::parse(query);
    let mut ranked = channels
        .into_iter()
        .filter(|(_, _, kind)| {
            matches!(
                kind,
                ConversationKind::PublicChannel | ConversationKind::PrivateChannel
            )
        })
        .filter_map(|(id, title, kind)| {
            let score = query.score([
                SearchField::new(&title, PRIMARY_FIELD_WEIGHT),
                SearchField::new(&id, ID_FIELD_WEIGHT),
            ])?;
            Some((
                score.band(),
                crate::search::normalize_for_search(&title),
                DirectoryRow {
                    title,
                    detail: gettext(kind.accessible_name()),
                    target: DirectoryTarget::Conversation(id),
                    icon_name: kind.icon_name(),
                },
            ))
        })
        .collect::<Vec<_>>();
    rank_directory_rows(&mut ranked)
}

/// Best relevance band first, then alphabetical, capped.
fn rank_directory_rows(ranked: &mut [(u8, String, DirectoryRow)]) -> Vec<DirectoryRow> {
    ranked.sort_by(
        |(left_band, left_key, left), (right_band, right_key, right)| {
            right_band
                .cmp(left_band)
                .then_with(|| left_key.cmp(right_key))
                .then_with(|| left.title.cmp(&right.title))
        },
    );
    ranked
        .iter()
        .take(MAX_DIRECTORY_ROWS)
        .map(|(_, _, row)| row.clone())
        .collect()
}

/// Filters picked above the search results. They are sent to Slack as query
/// modifiers, so Slack does the filtering and the typed text stays untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SearchFilters {
    pub(crate) direct_messages_only: bool,
    /// User id whose messages to find.
    pub(crate) from_user: Option<String>,
    /// Ready-made `in:` modifier, see [`conversation_modifier`].
    pub(crate) conversation_modifier: Option<String>,
    /// Day (`YYYY-MM-DD`) after which messages must have been posted.
    pub(crate) after: Option<String>,
}

impl SearchFilters {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The `typed` query followed by one modifier per active filter.
    pub(crate) fn api_query(&self, typed: &str) -> String {
        let mut parts = vec![typed.trim().to_string()];
        if self.direct_messages_only {
            parts.push("is:dm".to_string());
        }
        if let Some(user_id) = &self.from_user {
            parts.push(format!("from:<@{user_id}>"));
        }
        parts.extend(self.conversation_modifier.clone());
        if let Some(day) = &self.after {
            parts.push(format!("after:{day}"));
        }
        parts.retain(|part| !part.is_empty());
        parts.join(" ")
    }
}

/// The `in:` modifier that limits a search to one conversation. A direct
/// message is addressed by its other person.
pub(crate) fn conversation_modifier(
    kind: ConversationKind,
    id: &str,
    other_user_id: Option<&str>,
) -> Option<String> {
    match kind {
        ConversationKind::DirectMessage => other_user_id.map(|user_id| format!("in:<@{user_id}>")),
        ConversationKind::Unknown => None,
        _ => Some(format!("in:<#{id}>")),
    }
}

/// Search rows in API rank order, with the query terms highlighted.
pub(crate) fn search_rows(
    results: &[SearchMatch],
    query: &str,
    known_channel_title: &dyn Fn(&str) -> Option<String>,
    context: &MessageRenderContext,
) -> Vec<SearchRow> {
    let terms = highlight_terms(query);
    results
        .iter()
        .map(|result| {
            let channel_title = result
                .channel
                .as_ref()
                .and_then(|channel| {
                    channel
                        .id
                        .as_deref()
                        .and_then(known_channel_title)
                        .or_else(|| channel.name.as_deref().map(|name| format!("#{name}")))
                })
                .unwrap_or_else(|| "Slack".to_string());
            let author = result
                .user
                .as_deref()
                .and_then(|user_id| context.user_names.get(user_id).cloned())
                .or_else(|| result.username.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| gettext("Unknown"));
            let plain_text = plain_text(result.text.as_deref().unwrap_or_default(), context);
            SearchRow {
                location: result.message_location(),
                channel_title,
                author,
                author_user_id: result.user.clone().filter(|id| !id.trim().is_empty()),
                ts: result.ts.clone(),
                snippet_markup: highlight_markup(&plain_text, &terms),
                plain_text,
                permalink: result.permalink.clone().filter(|url| is_http_url(url)),
            }
        })
        .collect()
}

/// Normalized (case and accent folded) content terms of a Slack search query: modifiers such as
/// `in:#general` or `-excluded` are not highlighted.
pub(crate) fn highlight_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split_whitespace()
        .filter(|term| !term.starts_with('-') && !term.contains(':'))
        .map(|term| normalize_for_search(term.trim_matches(['"', '*'])))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| std::cmp::Reverse(term.chars().count()));
    terms.dedup();
    terms
}

/// Escapes `text` for Pango and wraps term matches in `<b>`. Matching goes
/// through `normalize_for_search`, so case and accents never matter (`a`
/// highlights `á`); matches map back to the original characters.
pub(crate) fn highlight_markup(text: &str, terms: &[String]) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    // Normalized characters, each tagged with the original character it came
    // from (one original can normalize to several, or to none).
    let mut normalized = Vec::with_capacity(chars.len());
    let mut origins = Vec::with_capacity(chars.len());
    let mut buffer = [0u8; 4];
    for (index, character) in chars.iter().enumerate() {
        for folded in normalize_for_search(character.encode_utf8(&mut buffer)).chars() {
            normalized.push(folded);
            origins.push(index);
        }
    }
    let mut highlighted = vec![false; chars.len()];
    for term in terms {
        let term_chars = normalize_for_search(term).chars().collect::<Vec<_>>();
        if term_chars.is_empty() || term_chars.len() > normalized.len() {
            continue;
        }
        for start in 0..=normalized.len() - term_chars.len() {
            if normalized[start..start + term_chars.len()] == term_chars[..] {
                for &origin in &origins[start..start + term_chars.len()] {
                    highlighted[origin] = true;
                }
            }
        }
    }
    let mut markup = String::with_capacity(text.len() + 16);
    let mut open = false;
    for (index, character) in chars.iter().enumerate() {
        if highlighted[index] != open {
            markup.push_str(if open { "</b>" } else { "<b>" });
            open = highlighted[index];
        }
        push_escaped_markup(&mut markup, *character);
    }
    if open {
        markup.push_str("</b>");
    }
    markup
}

fn push_escaped_markup(output: &mut String, character: char) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        '"' => output.push_str("&quot;"),
        '\'' => output.push_str("&#39;"),
        _ => output.push(character),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SlackSearchChannel;
    use crate::secondary_views::model::tests::context;

    #[test]
    fn filters_become_query_modifiers_after_the_typed_text() {
        assert!(SearchFilters::default().is_empty());
        assert_eq!(SearchFilters::default().api_query(" deploy "), "deploy");
        let filters = SearchFilters {
            direct_messages_only: true,
            from_user: Some("U1".to_string()),
            conversation_modifier: Some("in:<#C2>".to_string()),
            after: Some("2026-10-29".to_string()),
        };
        assert!(!filters.is_empty());
        assert_eq!(
            filters.api_query("deploy"),
            "deploy is:dm from:<@U1> in:<#C2> after:2026-10-29"
        );
        assert_eq!(
            filters.api_query(""),
            "is:dm from:<@U1> in:<#C2> after:2026-10-29"
        );
    }

    #[test]
    fn people_rows_match_names_aliases_and_accents_best_first() {
        let names = HashMap::from([
            ("U1".to_string(), "Zoë".to_string()),
            ("U2".to_string(), "Zoey".to_string()),
            ("U3".to_string(), "Ann".to_string()),
        ]);
        let full = HashMap::from([("U1".to_string(), "Zoë Smith".to_string())]);
        let aliases = UserSearchAliases::from([("U3".to_string(), vec!["zo-alias".to_string()])]);
        let rows = people_rows(&names, &full, &aliases, "zoe");
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["Zoë", "Zoey"]);
        assert_eq!(rows[0].detail, "Zoë Smith");
        assert_eq!(rows[0].target, DirectoryTarget::Person("U1".to_string()));
        assert_eq!(people_rows(&names, &full, &aliases, "zo-al").len(), 1);
        assert_eq!(people_rows(&names, &full, &aliases, "").len(), 3);
        assert!(people_rows(&names, &full, &aliases, "nobody").is_empty());
    }

    #[test]
    fn channel_rows_list_only_channels_matching_the_query() {
        let channels = vec![
            (
                "C1".to_string(),
                "#general".to_string(),
                ConversationKind::PublicChannel,
            ),
            (
                "C2".to_string(),
                "#secret".to_string(),
                ConversationKind::PrivateChannel,
            ),
            (
                "D1".to_string(),
                "General Bob".to_string(),
                ConversationKind::DirectMessage,
            ),
        ];
        let rows = channel_rows(channels.clone(), "gene");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].target,
            DirectoryTarget::Conversation("C1".to_string())
        );
        assert_eq!(channel_rows(channels, "").len(), 2);
    }

    #[test]
    fn search_scope_follows_the_type_dropdown() {
        assert_eq!(SearchScope::from_index(0), SearchScope::Messages);
        assert_eq!(SearchScope::from_index(1), SearchScope::DirectMessages);
        assert!(SearchScope::from_index(2).is_local());
        assert!(SearchScope::from_index(3).is_local());
        assert!(!SearchScope::from_index(1).is_local());
    }

    #[test]
    fn conversation_modifier_addresses_direct_messages_by_person() {
        assert_eq!(
            conversation_modifier(ConversationKind::PublicChannel, "C1", None).as_deref(),
            Some("in:<#C1>")
        );
        assert_eq!(
            conversation_modifier(ConversationKind::DirectMessage, "D1", Some("U9")).as_deref(),
            Some("in:<@U9>")
        );
        assert_eq!(
            conversation_modifier(ConversationKind::DirectMessage, "D1", None),
            None
        );
        assert_eq!(
            conversation_modifier(ConversationKind::Unknown, "X", None),
            None
        );
    }

    #[test]
    fn highlight_terms_skip_modifiers_and_strip_wildcards() {
        assert_eq!(
            highlight_terms("Deploy in:#ops -draft \"roll*\" from:@bob x"),
            ["deploy", "roll", "x"]
        );
    }

    #[test]
    fn highlight_markup_is_case_insensitive_and_escaped() {
        let terms = highlight_terms("deploy");
        assert_eq!(
            highlight_markup("<Deploy> & DEPLOY", &terms),
            "&lt;<b>Deploy</b>&gt; &amp; <b>DEPLOY</b>"
        );
        assert_eq!(highlight_markup("no match", &terms), "no match");
    }

    #[test]
    fn highlight_markup_ignores_accents_and_maps_back_to_original_text() {
        let terms = highlight_terms("a CAFE");
        assert_eq!(
            highlight_markup("Más Café & más", &terms),
            "M<b>á</b>s <b>Café</b> &amp; m<b>á</b>s"
        );
    }

    #[test]
    fn highlight_markup_merges_overlapping_terms() {
        let terms = highlight_terms("ab bc");
        assert_eq!(highlight_markup("xabcx", &terms), "x<b>abc</b>x");
    }

    #[test]
    fn search_rows_resolve_channel_author_and_safe_permalink() {
        let results = vec![
            SearchMatch {
                channel: Some(SlackSearchChannel {
                    id: Some("C1".to_string()),
                    name: Some("general".to_string()),
                }),
                user: Some("U2".to_string()),
                text: Some("ship <@U1> *now*".to_string()),
                ts: Some("10.0".to_string()),
                permalink: Some("https://example.slack.com/archives/C1/p10".to_string()),
                ..SearchMatch::default()
            },
            SearchMatch {
                channel: Some(SlackSearchChannel {
                    id: Some("C9".to_string()),
                    name: Some("random".to_string()),
                }),
                username: Some("bot".to_string()),
                text: Some("ship".to_string()),
                permalink: Some("javascript:alert(1)".to_string()),
                ..SearchMatch::default()
            },
        ];
        let known = |id: &str| (id == "C1").then(|| "General".to_string());
        let rows = search_rows(&results, "ship", &known, &context());

        assert_eq!(rows[0].channel_title, "General");
        assert_eq!(rows[0].author, "Bob");
        assert_eq!(rows[0].author_user_id.as_deref(), Some("U2"));
        assert_eq!(rows[1].author_user_id, None);
        assert!(rows[0].plain_text.contains("@Alice"));
        assert!(rows[0].snippet_markup.starts_with("<b>ship</b>"));
        assert!(rows[0].location.is_some());
        assert!(rows[0].permalink.is_some());
        assert_eq!(rows[1].channel_title, "#random");
        assert_eq!(rows[1].author, "bot");
        assert!(rows[1].location.is_none());
        assert!(rows[1].permalink.is_none());
    }
}
