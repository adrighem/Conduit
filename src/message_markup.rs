//! Slack mrkdwn to Pango markup, plus the per-render [`MessageRenderContext`]
//! that carries the workspace catalogs every message renderer reads.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config;
use crate::emoji::{EmojiCatalog, EmojiEntry};
use crate::models::{SlackMessage, SlackUserStatus};
use emoji::render_emoji_shortcode_pango;
pub use emoji::{emoji_only_names, INLINE_EMOJI_SIZE};

mod emoji;
#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
mod tests;

const QUICK_REACTION_LIMIT: usize = 4;

/// Canonical render context for Slack message formatting and timeline display.
///
/// The workspace directories (users, groups, conversations, emoji) are
/// `Arc`-shared snapshots that the window reuses until their source changes.
/// Cloning a context only bumps their refcounts, and the derived `PartialEq`
/// checks them by pointer before contents (`Arc<T: Eq>` short-circuits on
/// `Arc::ptr_eq`), so comparing two renders of an unchanged workspace only
/// walks the per-render fields (thread, unread anchor, images).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageRenderContext {
    pub user_names: Arc<HashMap<String, String>>,
    pub user_full_names: Arc<HashMap<String, String>>,
    pub user_avatar_urls: Arc<HashMap<String, String>>,
    /// Directory users flagged `is_bot`; see [`Self::display_user_id`].
    pub bot_user_ids: Arc<HashSet<String>>,
    pub conversation_titles: Arc<HashMap<String, String>>,
    pub private_conversation_ids: Arc<HashSet<String>>,
    pub user_statuses: Arc<HashMap<String, SlackUserStatus>>,
    pub user_group_names: Arc<HashMap<String, String>>,
    pub user_group_members: Arc<HashMap<String, Vec<String>>>,
    pub current_user_id: Option<String>,
    pub thread_ts: Option<String>,
    pub timeline_scroll: TimelineScrollBehavior,
    /// Image source key (Slack URL) -> file the runtime cached for it.
    pub image_assets: HashMap<String, PathBuf>,
    pub failed_image_urls: HashSet<String>,
    pub recent_reactions: Vec<String>,
    pub custom_emojis: Arc<HashMap<String, String>>,
    pub last_read: Option<String>,
    /// Frozen "first unread message" anchor for the unread separator line.
    /// Computed once per conversation/thread-open and held fixed across
    /// re-renders - see `ConduitWindow::unread_separator_anchor_ts`.
    pub unread_separator_ts: Option<String>,
}

impl MessageRenderContext {
    /// Whether rows bound against `self` render the same against `other`,
    /// so a re-render can keep them. Fields rows never read, and fields with
    /// a targeted update path, are left out; the destructuring makes every
    /// new field pick a side.
    pub(crate) fn same_row_inputs(&self, other: &Self) -> bool {
        let Self {
            user_names,
            user_full_names,
            user_avatar_urls,
            bot_user_ids,
            conversation_titles,
            private_conversation_ids,
            user_statuses,
            user_group_names,
            user_group_members,
            current_user_id,
            thread_ts,
            recent_reactions,
            custom_emojis,
            // Scroll intent of one render, read by the view only.
            timeline_scroll: _,
            // `update_image_asset` re-renders rows once an asset arrives or
            // fails, so a re-render need not.
            image_assets: _,
            failed_image_urls: _,
            // Rows read the derived `unread_separator_ts` only.
            last_read: _,
            // Moving the separator changes only the rows that gain or lose
            // it; `set_messages` and `set_unread_separator` rebind just those.
            unread_separator_ts: _,
        } = self;
        user_names == &other.user_names
            && user_full_names == &other.user_full_names
            && user_avatar_urls == &other.user_avatar_urls
            && bot_user_ids == &other.bot_user_ids
            && conversation_titles == &other.conversation_titles
            && private_conversation_ids == &other.private_conversation_ids
            && user_statuses == &other.user_statuses
            && user_group_names == &other.user_group_names
            && user_group_members == &other.user_group_members
            && current_user_id == &other.current_user_id
            && thread_ts == &other.thread_ts
            && recent_reactions == &other.recent_reactions
            && custom_emojis == &other.custom_emojis
    }

    /// The user whose name and avatar head `message`: its author, or for an
    /// app-posted message the invoking person (e.g. `/giphy`), provided the
    /// directory knows that user and does not flag them as a bot. Anything
    /// else keeps the app identity.
    pub(crate) fn display_user_id<'message>(
        &self,
        message: &'message SlackMessage,
    ) -> Option<&'message str> {
        message.author_user_id().or_else(|| {
            message.app_invoking_user_id().filter(|user_id| {
                !self.bot_user_ids.contains(*user_id)
                    && (self.user_full_names.contains_key(*user_id)
                        || self.user_names.contains_key(*user_id))
            })
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineScrollBehavior {
    #[default]
    Preserve,
    PreservePrepend,
    Bottom,
    StickToBottom,
}

/// Prefix of user group mention links; the timeline opens the group's members.
pub const USER_GROUP_URL_PREFIX: &str = "conduit-usergroup://";

/// Prefix of message target action links, which stay clickable in
/// message text; the window opens them in Conduit.
pub const MESSAGE_TARGET_URL_PREFIX: &str = "conduit://message?";

pub(crate) fn recent_reactions(context: &MessageRenderContext) -> Vec<EmojiEntry> {
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let mut usage = HashMap::new();
    for (index, name) in context
        .recent_reactions
        .iter()
        .take(config::RECENT_REACTION_HISTORY_LIMIT)
        .map(|name| catalog.canonical_name(name))
        .enumerate()
    {
        if name.trim().is_empty() {
            continue;
        }
        let (count, _) = usage.entry(name).or_insert((0usize, index));
        *count += 1;
    }
    let mut ranked = usage.into_iter().collect::<Vec<_>>();
    ranked.sort_by(
        |(left_name, (left_count, left_recency)), (right_name, (right_count, right_recency))| {
            right_count
                .cmp(left_count)
                .then_with(|| left_recency.cmp(right_recency))
                .then_with(|| left_name.cmp(right_name))
        },
    );
    let requested = ranked.into_iter().map(|(name, _)| name).chain([
        "smile".to_string(),
        "+1".to_string(),
        "white_check_mark".to_string(),
        "heart".to_string(),
    ]);
    let mut seen = HashSet::new();
    requested
        .filter(|name| seen.insert(name.clone()))
        .filter_map(|name| emoji_entry(&name, context))
        .take(QUICK_REACTION_LIMIT)
        .collect()
}

fn emoji_entry(name: &str, context: &MessageRenderContext) -> Option<EmojiEntry> {
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let value = catalog.resolve(name)?;
    let label = emojis::get_by_shortcode(name)
        .map(|emoji| emoji.name().to_string())
        .unwrap_or_else(|| name.replace(['_', '-'], " "));
    Some(EmojiEntry {
        name: name.to_string(),
        label,
        category: "",
        value,
    })
}

pub fn mrkdwn_to_pango(text: &str, context: &MessageRenderContext) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        output.push_str(&mrkdwn_to_pango_inline(&rest[..start], context));
        rest = &rest[start + 3..];
        if let Some(end) = rest.find("```") {
            output.push_str("<tt>");
            output.push_str(&escape_pango(&rest[..end]));
            output.push_str("</tt>");
            rest = &rest[end + 3..];
        } else {
            output.push_str(&escape_pango("```"));
            output.push_str(&mrkdwn_to_pango_inline(rest, context));
            rest = "";
        }
    }

    output.push_str(&mrkdwn_to_pango_inline(rest, context));
    output
}

fn mrkdwn_to_pango_inline(text: &str, context: &MessageRenderContext) -> String {
    let mut output = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some((character, consumed)) = decode_html_entity_prefix(rest) {
            push_escaped_pango_character(&mut output, character);
            rest = &rest[consumed..];
            continue;
        }
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                output.push_str("<tt>");
                output.push_str(&escape_pango(&rest[1..1 + end]));
                output.push_str("</tt>");
                rest = &rest[end + 2..];
                continue;
            }
        }

        if let Some((pango, consumed)) = render_slack_entity_pango(rest, context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((pango, consumed)) = render_bare_channel_reference_pango(rest, context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((pango, consumed)) = render_emoji_shortcode_pango(rest, context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((pango, consumed)) = render_wrapped_pango(rest, '*', "b", context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((pango, consumed)) = render_wrapped_pango(rest, '_', "i", context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((pango, consumed)) = render_wrapped_pango(rest, '~', "s", context) {
            output.push_str(&pango);
            rest = &rest[consumed..];
            continue;
        }

        let next = rest.chars().next().expect("non-empty string has a char");
        if next == '\n' {
            output.push('\n');
        } else {
            push_escaped_pango_character(&mut output, next);
        }
        rest = &rest[next.len_utf8()..];
    }

    output
}

fn mention_pill_pango(prefix: &str, label: &str) -> String {
    format!(
        "<span background=\"#D6ECFF\" foreground=\"#1264A3\"> {}{} </span>",
        prefix,
        escape_pango(label)
    )
}

fn render_slack_entity_pango(
    text: &str,
    context: &MessageRenderContext,
) -> Option<(String, usize)> {
    if !text.starts_with('<') {
        return None;
    }

    let end = text.find('>')?;
    let raw = &text[1..end];
    let rendered = if let Some(user) = raw.strip_prefix('@') {
        let (user_id, fallback) = user
            .split_once('|')
            .map_or((user, None), |(user_id, label)| (user_id, Some(label)));
        let name = context
            .user_names
            .get(user_id)
            .cloned()
            .or_else(|| fallback.map(ToString::to_string))
            .unwrap_or_else(|| user_id.to_string());
        format!(
            "<a href=\"conduit-user://{}\">{}</a>",
            escape_pango(user_id),
            mention_pill_pango("@", &name)
        )
    } else if raw.starts_with("!subteam^") {
        user_group_mention_pango(raw, context)
    } else if let Some(channel) = raw.strip_prefix('#') {
        let (channel_id, fallback) = channel
            .split_once('|')
            .map_or((channel, None), |(channel_id, label)| {
                (channel_id, Some(label))
            });
        let raw_display = context
            .conversation_titles
            .get(channel_id)
            .map(|s| s.strip_prefix('#').unwrap_or(s).to_string())
            .unwrap_or_else(|| {
                fallback
                    .filter(|f| !f.trim().is_empty())
                    .unwrap_or(channel_id)
                    .to_string()
            });
        let clean_display = raw_display.strip_prefix('#').unwrap_or(&raw_display);
        let display = if clean_display.trim().is_empty() {
            channel_id.strip_prefix('#').unwrap_or(channel_id)
        } else {
            clean_display
        };
        let prefix = if context.private_conversation_ids.contains(channel_id) {
            "🔒 "
        } else {
            "#"
        };
        mention_pill_pango(prefix, display)
    } else if let Some((url, label)) = raw.split_once('|') {
        if is_http_url(url) || url.starts_with(MESSAGE_TARGET_URL_PREFIX) {
            external_link_pango(url, label)
        } else {
            escape_pango(label)
        }
    } else if raw.starts_with('!') {
        slack_special_entity_pango(raw)
    } else if is_http_url(raw) {
        external_link_pango(raw, raw)
    } else {
        return None;
    };

    Some((rendered, end + 1))
}

fn render_bare_channel_reference_pango(
    text: &str,
    context: &MessageRenderContext,
) -> Option<(String, usize)> {
    let candidate = text.strip_prefix('#')?;
    let id_length = candidate
        .bytes()
        .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        .count();
    if id_length == 0 {
        return None;
    }
    let channel_id = &candidate[..id_length];
    let title = context.conversation_titles.get(channel_id)?;
    let display = title.strip_prefix('#').unwrap_or(title);
    let prefix = if context.private_conversation_ids.contains(channel_id) {
        "🔒 "
    } else {
        "#"
    };
    Some((mention_pill_pango(prefix, display), id_length + 1))
}

fn user_group_mention_pango(raw: &str, context: &MessageRenderContext) -> String {
    let Some(group) = raw.strip_prefix("!subteam^") else {
        return escape_pango(raw);
    };
    let (group_id, fallback_label) = group
        .split_once('|')
        .map(|(group_id, label)| (group_id, Some(normalized_user_group_label(label))))
        .unwrap_or((group, None));

    let label = context
        .user_group_names
        .get(group_id)
        .cloned()
        .or(fallback_label)
        .unwrap_or_else(|| group_id.to_string());
    let label = normalized_user_group_label(&label);
    format!(
        "<a href=\"{USER_GROUP_URL_PREFIX}{}\">{}</a>",
        escape_pango(group_id),
        mention_pill_pango("@", &label)
    )
}

fn slack_special_entity_pango(raw: &str) -> String {
    if let Some((_, label)) = raw.rsplit_once('|') {
        return escape_pango(label);
    }

    match raw {
        "!channel" => "<span weight=\"bold\">@channel</span>".to_string(),
        "!here" => "<span weight=\"bold\">@here</span>".to_string(),
        "!everyone" => "<span weight=\"bold\">@everyone</span>".to_string(),
        _ => escape_pango(raw),
    }
}

fn external_link_pango(url: &str, label: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>",
        escape_pango(url),
        escape_pango(label)
    )
}

fn render_wrapped_pango(
    text: &str,
    marker: char,
    tag: &str,
    context: &MessageRenderContext,
) -> Option<(String, usize)> {
    if !text.starts_with(marker) {
        return None;
    }

    let marker_len = marker.len_utf8();
    let end = text[marker_len..].find(marker)?;
    let inner = &text[marker_len..marker_len + end];
    if inner.trim().is_empty() {
        return None;
    }

    Some((
        format!("<{tag}>{}</{tag}>", mrkdwn_to_pango_inline(inner, context)),
        marker_len + end + marker_len,
    ))
}

pub(super) fn escape_pango(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((character, consumed)) = decode_html_entity_prefix(rest) {
            push_escaped_pango_character(&mut escaped, character);
            rest = &rest[consumed..];
            continue;
        }

        let character = rest.chars().next().expect("non-empty text has a character");
        push_escaped_pango_character(&mut escaped, character);
        rest = &rest[character.len_utf8()..];
    }
    escaped
}

fn push_escaped_pango_character(output: &mut String, character: char) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        '\'' => output.push_str("&apos;"),
        '"' => output.push_str("&quot;"),
        c => output.push(c),
    }
}

fn normalized_user_group_label(label: &str) -> String {
    label.trim().trim_start_matches('@').to_string()
}

pub(super) fn is_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

pub(crate) fn decode_html_entity_prefix(text: &str) -> Option<(char, usize)> {
    if !text.starts_with('&') {
        return None;
    }
    let end = text
        .as_bytes()
        .iter()
        .take(16)
        .position(|byte| *byte == b';')?;
    let entity = &text[1..end];
    let character = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00a0}',
        entity if entity.starts_with("#x") || entity.starts_with("#X") => {
            char::from_u32(u32::from_str_radix(&entity[2..], 16).ok()?)?
        }
        entity if entity.starts_with('#') => char::from_u32(entity[1..].parse().ok()?)?,
        _ => return None,
    };
    Some((character, end + 1))
}
