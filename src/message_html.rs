use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use gettextrs::gettext;
use serde::Serialize;

use crate::config;
use crate::day_label::{capitalize_first_letter, date_text};
use crate::emoji::{EmojiCatalog, EmojiEntry, EmojiValue};
use crate::message_handoff::{MessageControlHandle, MessageRef};
use crate::models::{SlackMessage, SlackUserStatus};
use crate::rich_message::MessageControlKey;

#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
mod tests;

const CACHED_ASSET_URI_PREFIX: &str = "conduit-asset://";
const QUICK_REACTION_LIMIT: usize = 4;
static TIME_FORMAT_LOCALE: OnceLock<Option<String>> = OnceLock::new();

/// Canonical render context for Slack message formatting and timeline display.
pub type MessageRenderContext = MessageHtmlContext;

#[derive(Debug, Clone, Default)]
pub struct MessageHtmlContext {
    pub user_names: Arc<HashMap<String, String>>,
    pub user_full_names: Arc<HashMap<String, String>>,
    pub user_avatar_urls: Arc<HashMap<String, String>>,
    /// Directory users flagged `is_bot`; see [`Self::display_user_id`].
    pub bot_user_ids: Arc<HashSet<String>>,
    pub conversation_titles: HashMap<String, String>,
    pub private_conversation_ids: HashSet<String>,
    pub user_statuses: Arc<HashMap<String, SlackUserStatus>>,
    pub user_group_names: Arc<HashMap<String, String>>,
    pub user_group_members: Arc<HashMap<String, Vec<String>>>,
    pub current_user_id: Option<String>,
    pub thread_ts: Option<String>,
    pub load_more_url: Option<String>,
    pub timeline_scroll: TimelineScrollBehavior,
    pub image_assets: HashMap<String, CachedAssetSource>,
    pub failed_image_urls: HashSet<String>,
    pub recent_reactions: Vec<String>,
    pub custom_emojis: Arc<HashMap<String, String>>,
    pub timeline_generation: Option<u64>,
    pub last_read: Option<String>,
    /// Frozen "first unread message" anchor for the unread separator line.
    /// Computed once per conversation/thread-open and held fixed across
    /// re-renders - see `ConduitWindow::unread_separator_anchor_ts`.
    pub unread_separator_ts: Option<String>,
    pub(crate) message_control_handles: HashMap<MessageRef, MessageControlHandle>,
    pub(crate) message_control_action_handles:
        HashMap<(MessageRef, MessageControlKey), MessageControlHandle>,
}

impl MessageHtmlContext {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CachedAssetKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedAssetSource {
    uri: String,
    kind: CachedAssetKind,
}

impl CachedAssetSource {
    pub fn new(uri: impl Into<String>, kind: CachedAssetKind) -> Option<Self> {
        let uri = uri.into();
        let key = uri.strip_prefix(CACHED_ASSET_URI_PREFIX)?;
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        Some(Self { uri, kind })
    }

    pub fn from_cache_key(cache_key: &str, kind: CachedAssetKind) -> Option<Self> {
        Self::new(format!("{CACHED_ASSET_URI_PREFIX}{cache_key}"), kind)
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn kind(&self) -> CachedAssetKind {
        self.kind
    }
}

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
    let days_old = crate::day_label::days_old(datetime, now);

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

pub fn load_more_action_url(channel_id: &str, cursor: &str, thread_ts: Option<&str>) -> String {
    let mut url = format!(
        "conduit://load-older?channel={}&cursor={}",
        encode_query(channel_id),
        encode_query(cursor)
    );

    append_thread_ts_query(&mut url, thread_ts);
    url
}

/// Prefix of message target action links, which stay clickable in
/// message text; the window opens them in Conduit.
pub const MESSAGE_TARGET_URL_PREFIX: &str = "conduit://message?";

fn append_thread_ts_query(url: &mut String, thread_ts: Option<&str>) {
    if let Some(thread_ts) = thread_ts.filter(|ts| !ts.is_empty()) {
        url.push_str("&thread_ts=");
        url.push_str(&encode_query(thread_ts));
    }
}

pub(crate) fn recent_reactions(context: &MessageHtmlContext) -> Vec<EmojiEntry> {
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

fn emoji_entry(name: &str, context: &MessageHtmlContext) -> Option<EmojiEntry> {
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

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

pub fn mrkdwn_to_pango(text: &str, context: &MessageHtmlContext) -> String {
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

fn mrkdwn_to_pango_inline(text: &str, context: &MessageHtmlContext) -> String {
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

fn render_slack_entity_pango(text: &str, context: &MessageHtmlContext) -> Option<(String, usize)> {
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
    context: &MessageHtmlContext,
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

fn user_group_mention_pango(raw: &str, context: &MessageHtmlContext) -> String {
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
    mention_pill_pango("@", &label)
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

fn render_emoji_shortcode_pango(
    text: &str,
    context: &MessageHtmlContext,
) -> Option<(String, usize)> {
    if !text.starts_with(':') {
        return None;
    }

    let end = text[1..].find(':')? + 1;
    let code = &text[1..end];
    if code.is_empty()
        || code.len() > 64
        || !code.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        })
    {
        return None;
    }

    let mut consumed = end + 1;
    let mut resolved_code = code.to_string();
    if let Some((modifier, modifier_len)) = adjacent_skin_tone_shortcode(&text[consumed..]) {
        resolved_code.push_str("::");
        resolved_code.push_str(modifier);
        consumed += modifier_len;
    }
    let shortcode = &text[..consumed];
    let emoji = EmojiCatalog::new(&context.custom_emojis).resolve(&resolved_code);

    let rendered = match emoji {
        Some(EmojiValue::Unicode(val)) => escape_pango(val),
        Some(EmojiValue::CustomImage(ref url)) => {
            format!(
                "<conduit-custom-emoji name=\"{}\" url=\"{}\"/>",
                escape_pango(&resolved_code),
                escape_pango(url)
            )
        }
        _ => escape_pango(shortcode),
    };

    Some((rendered, consumed))
}

fn render_wrapped_pango(
    text: &str,
    marker: char,
    tag: &str,
    context: &MessageHtmlContext,
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

fn escape_pango(text: &str) -> String {
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

fn adjacent_skin_tone_shortcode(text: &str) -> Option<(&str, usize)> {
    if !text.starts_with(":skin-tone-") {
        return None;
    }
    let end = text[1..].find(':')? + 1;
    let modifier = &text[1..end];
    if modifier.len() > 64
        || !modifier.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        })
    {
        return None;
    }
    Some((modifier, end + 1))
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
