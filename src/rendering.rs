use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::message_markup::decode_html_entity_prefix;
use crate::models::{SlackFile, SlackMessage};
use std::collections::HashMap;

pub fn extract_user_ids(message: &SlackMessage) -> Vec<String> {
    let mut ids = Vec::new();
    // App messages also load their invoking user so the directory can tell
    // a slash-command person from a bot user (see `display_user_id`).
    for user in [message.author_user_id(), message.app_invoking_user_id()]
        .into_iter()
        .flatten()
    {
        ids.push(user.to_string());
    }
    if message.content_version == crate::rich_message::MESSAGE_CONTENT_VERSION {
        ids.extend(
            message
                .document
                .mentioned_user_ids()
                .map(ToString::to_string),
        );
    }
    extract_mentions(&message.visible_text(), &mut ids);
    ids.extend(
        message
            .reactions
            .as_ref()
            .into_iter()
            .flatten()
            .flat_map(|reaction| reaction.users.as_ref().into_iter().flatten().cloned()),
    );
    ids.sort();
    ids.dedup();
    ids
}

/// Ids of `<#C...>` channel mentions that carry no `|label` fallback, so the
/// title has to come from the channel catalog.
pub fn extract_channel_ids(message: &SlackMessage) -> Vec<String> {
    let text = message.visible_text();
    let mut ids = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<#") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let (id, label) = rest[..end].split_once('|').unwrap_or((&rest[..end], ""));
        let id = id.trim();
        let valid = !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
        if valid && label.trim().is_empty() {
            ids.push(id.to_string());
        }
        rest = &rest[end + 1..];
    }
    ids.sort();
    ids.dedup();
    ids
}

pub fn extract_mentions(text: &str, ids: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            return;
        };
        let user_id = rest[..end].split('|').next().unwrap_or_default().trim();
        if !user_id.is_empty() {
            ids.push(user_id.to_string());
        }
        rest = &rest[end + 1..];
    }
}

/// Whether `message` shows image asset `key`: avatar, file preview, unfurl,
/// document image or custom emoji (reaction or `:name:` in text).
pub(crate) fn message_uses_image_asset(
    message: &SlackMessage,
    catalog: &EmojiCatalog,
    key: &str,
) -> bool {
    message_uses_image_asset_matching(message, catalog, |candidate| candidate == key)
}

/// [`message_uses_image_asset`] for any key `is_key` accepts, so a batch of
/// arrived assets is matched in one pass over the messages.
pub(crate) fn message_uses_image_asset_matching(
    message: &SlackMessage,
    catalog: &EmojiCatalog,
    is_key: impl Fn(&str) -> bool,
) -> bool {
    message.avatar_url().is_some_and(&is_key)
        || message
            .display_files()
            .flat_map(SlackFile::preview_candidates)
            .filter_map(bounded_image_asset_request)
            .any(|(candidate, _)| is_key(&candidate))
        || message
            .attachments
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(attachment_image_asset_request)
            .any(|(candidate, _)| is_key(&candidate))
        || message
            .document
            .image_urls()
            .any(&is_key)
        || message
            .reactions
            .as_ref()
            .into_iter()
            .flatten()
            .any(|r| {
                r.name.as_deref().is_some_and(|name| {
                    let clean_name = name.trim_matches(':');
                    matches!(catalog.resolve(clean_name), Some(crate::emoji::EmojiValue::CustomImage(ref url)) if is_key(url))
                })
            })
        || any_custom_emoji_url_in_message(message, catalog, &is_key)
}

pub(crate) fn any_custom_emoji_url_in_message(
    message: &SlackMessage,
    catalog: &EmojiCatalog,
    mut visit: impl FnMut(&str) -> bool,
) -> bool {
    if let Some(text) = message.text.as_deref() {
        if any_custom_emoji_url_in_text(text, catalog, &mut visit) {
            return true;
        }
    }
    if !message.document.is_empty() {
        let visible = message.visible_text();
        if any_custom_emoji_url_in_text(&visible, catalog, &mut visit) {
            return true;
        }
    }
    false
}

fn any_custom_emoji_url_in_text(
    text: &str,
    catalog: &EmojiCatalog,
    visit: &mut impl FnMut(&str) -> bool,
) -> bool {
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find(':') else {
            break;
        };
        let code = &rest[..end];
        let valid_shortcode = !code.is_empty()
            && code.len() <= 64
            && code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'));
        if valid_shortcode {
            if let Some(crate::emoji::EmojiValue::CustomImage(ref url)) = catalog.resolve(code) {
                if visit(url) {
                    return true;
                }
            }
            rest = &rest[end + 1..];
        } else {
            // Keep the second colon as a potential opening delimiter (e.g. after "https:" or "12:30").
            rest = &rest[end..];
        }
    }
    false
}

pub(crate) fn image_asset_request(file: &SlackFile) -> Option<(String, String)> {
    file.preview_candidates()
        .find_map(bounded_image_asset_request)
}

pub(crate) fn attachment_image_asset_request(
    attachment: &crate::models::SlackAttachment,
) -> Option<(String, String)> {
    let url = attachment
        .image_url
        .as_deref()
        .or(attachment.thumb_url.as_deref())?;
    document_image_asset_request(url)
}

/// Document images (blocks, unfurls) keep the original URL as cache key but
/// fetch via `preview_fetch_url`: direct for allowlisted hosts, otherwise
/// through Slack's image proxy. Must agree with `timeline_media::fetchable_url`.
pub(crate) fn document_image_asset_request(url: &str) -> Option<(String, String)> {
    let (key, _) = bounded_image_asset_request(url)?;
    let fetch_url = crate::image_proxy::preview_fetch_url(url)?;
    Some((key, fetch_url))
}

pub(crate) fn bounded_image_asset_request(url: &str) -> Option<(String, String)> {
    if url.is_empty() || url.len() > crate::asset_server::IMAGE_ASSET_SOURCE_MAX_BYTES {
        return None;
    }
    Some((url.to_string(), url.to_string()))
}

/// Converts Slack mrkdwn and stray HTML into plain text for desktop notifications.
///
/// Returns `None` while a mentioned user is still unresolved so callers can
/// wait for names. Custom workspace emoji stay as `:name:` text.
pub fn notification_plain_text(
    text: &str,
    user_names: &HashMap<String, String>,
    user_full_names: &HashMap<String, String>,
) -> Option<String> {
    let custom = HashMap::new();
    let emoji = EmojiCatalog::new(&custom);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(character) = rest.chars().next() {
        let tail = &rest[character.len_utf8()..];
        match character {
            '<' => {
                let Some(end) = tail.find('>') else {
                    out.push('<');
                    rest = tail;
                    continue;
                };
                out.push_str(&plain_angle_token(
                    &tail[..end],
                    user_names,
                    user_full_names,
                )?);
                rest = &tail[end + 1..];
            }
            '&' => match decode_html_entity_prefix(rest) {
                Some((decoded, length)) => {
                    out.push(decoded);
                    rest = &rest[length..];
                }
                None => {
                    out.push('&');
                    rest = tail;
                }
            },
            ':' => match shortcode_emoji(tail, &emoji) {
                Some((unicode, length)) => {
                    out.push_str(unicode);
                    rest = &tail[length..];
                }
                None => {
                    out.push(':');
                    rest = tail;
                }
            },
            _ => {
                out.push(character);
                rest = tail;
            }
        }
    }

    let stripped = strip_mrkdwn_markers(&out.chars().collect::<Vec<_>>());
    Some(collapse_whitespace(&stripped))
}

/// Plain text for the inside of one `<...>` token, `None` for unresolved users.
fn plain_angle_token(
    inner: &str,
    user_names: &HashMap<String, String>,
    user_full_names: &HashMap<String, String>,
) -> Option<String> {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(label.trim()).filter(|label| !label.is_empty())),
        None => (inner, None),
    };
    if let Some(user_id) = target.strip_prefix('@') {
        let name = user_full_names
            .get(user_id.trim())
            .or_else(|| user_names.get(user_id.trim()))?
            .trim();
        return (!name.is_empty()).then(|| format!("@{name}"));
    }
    if let Some(channel) = target.strip_prefix('#') {
        return Some(format!("#{}", label.unwrap_or(channel)));
    }
    if let Some(special) = target.strip_prefix('!') {
        return Some(match special {
            "here" | "channel" | "everyone" => format!("@{special}"),
            _ if special.starts_with("subteam^") => label.map_or_else(
                || "@group".to_string(),
                |label| format!("@{}", label.trim_start_matches('@')),
            ),
            _ => label
                .unwrap_or_else(|| special.split('^').next().unwrap_or_default())
                .to_string(),
        });
    }
    let is_link = !target.contains(char::is_whitespace)
        && (target.contains("://") || target.starts_with("mailto:") || target.starts_with("tel:"));
    if is_link {
        return Some(notification_link_text(target, label));
    }
    // Anything else is an HTML tag: drop it, keeping line breaks.
    let tag = target.trim_start_matches('/').to_ascii_lowercase();
    let name = tag
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    Some(if matches!(name, "br" | "p" | "div" | "li") {
        "\n".to_string()
    } else {
        String::new()
    })
}

/// Notification text for a link. Notification servers only linkify bare URLs,
/// so web links always carry the full target: a label that is itself a (often
/// ellipsed) URL is dropped, any other label is kept ahead of the target.
fn notification_link_text(target: &str, label: Option<&str>) -> String {
    let Some(label) = label else {
        return target.to_string();
    };
    if !target.contains("://") {
        return label.to_string();
    }
    if label.contains("://") || label.ends_with('\u{2026}') || label.ends_with("...") {
        return target.to_string();
    }
    format!("{label} ({target})")
}

/// Resolves a leading `name:` shortcode (the opening colon already consumed).
fn shortcode_emoji<'a>(tail: &str, catalog: &EmojiCatalog<'a>) -> Option<(&'static str, usize)> {
    let length = tail
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')))
        .filter(|length| *length > 0 && tail[*length..].starts_with(':'))?;
    match catalog.resolve(&tail[..length])? {
        EmojiValue::Unicode(unicode) => Some((unicode, length + 1)),
        EmojiValue::CustomImage(_) => None,
    }
}

/// Removes paired `*`, `_`, `~`, backtick and fenced-code markers, keeping the inner text.
fn strip_mrkdwn_markers(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let marker = chars[i];
        if chars[i..].starts_with(&['`', '`', '`']) {
            if let Some(len) = chars[i + 3..].windows(3).position(|w| w == ['`'; 3]) {
                let block: String = chars[i + 3..i + 3 + len].iter().collect();
                out.push_str(block.trim());
                i += len + 6;
                continue;
            }
        }
        let opens = matches!(marker, '*' | '_' | '~' | '`')
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && chars
                .get(i + 1)
                .is_some_and(|c| !c.is_whitespace() && *c != marker);
        let close = opens
            .then(|| {
                (i + 2..chars.len())
                    .take_while(|j| chars[*j] != '\n')
                    .find(|j| {
                        chars[*j] == marker
                            && !chars[*j - 1].is_whitespace()
                            && chars.get(*j + 1).is_none_or(|c| !c.is_alphanumeric())
                    })
            })
            .flatten();
        match close {
            Some(j) if marker == '`' => out.extend(&chars[i + 1..j]),
            Some(j) => out.push_str(&strip_mrkdwn_markers(&chars[i + 1..j])),
            None => {
                out.push(marker);
                i += 1;
                continue;
            }
        }
        i = close.unwrap_or(i) + 1;
    }
    out
}

/// Trims lines, squeezes runs of spaces and keeps at most one blank line.
fn collapse_whitespace(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && lines.last().is_none_or(String::is_empty) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_author_mentions_and_reaction_user_ids() {
        let message = SlackMessage {
            user: Some("U123".to_string()),
            text: Some("hi <@U999> and <@U123>".to_string()),
            reactions: Some(vec![crate::models::SlackReaction {
                users: Some(vec!["U456".to_string(), "U123".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(
            extract_user_ids(&message),
            vec!["U123".to_string(), "U456".to_string(), "U999".to_string()]
        );
    }

    #[test]
    fn extracts_only_unlabelled_channel_mentions() {
        let message = SlackMessage {
            text: Some("see <#C1> and <#C2|ops>, <#C1>, <#c3> <#C4|> <#C5".to_string()),
            ..Default::default()
        };

        assert_eq!(
            extract_channel_ids(&message),
            vec!["C1".to_string(), "C4".to_string()]
        );
    }

    #[test]
    fn extracts_mentions_from_attachment_only_messages() {
        let message = SlackMessage {
            attachments: Some(vec![crate::models::SlackAttachment {
                text: Some("Review this with <@U999>".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(extract_user_ids(&message), vec!["U999".to_string()]);
    }

    #[test]
    fn resolves_user_mentions_to_display_names() {
        let names = std::collections::HashMap::from([
            ("U123".to_string(), "ada".to_string()),
            ("U456".to_string(), "grace".to_string()),
        ]);
        let full_names = std::collections::HashMap::from([
            ("U123".to_string(), "Ada Lovelace".to_string()),
            ("U456".to_string(), "Grace Hopper".to_string()),
        ]);

        assert_eq!(
            notification_plain_text("Hi <@U123>, meet <@U456|grace>.", &names, &full_names)
                .as_deref(),
            Some("Hi @Ada Lovelace, meet @Grace Hopper.")
        );
        assert_eq!(
            notification_plain_text("Hi <@U999>", &names, &full_names),
            None
        );
        assert_eq!(
            notification_plain_text("Malformed <@U123", &names, &full_names).as_deref(),
            Some("Malformed <@U123")
        );
    }

    fn plain(text: &str) -> Option<String> {
        let names = HashMap::from([("U123".to_string(), "Ada".to_string())]);
        let full_names = HashMap::new();
        notification_plain_text(text, &names, &full_names)
    }

    #[test]
    fn notification_text_resolves_mentions_and_specials() {
        assert_eq!(
            plain("hi <@U123> in <#C1|general> <!here>").as_deref(),
            Some("hi @Ada in #general @here")
        );
        assert_eq!(
            plain("<!channel> <!everyone> <#C9>").as_deref(),
            Some("@channel @everyone #C9")
        );
        assert_eq!(plain("<!subteam^S1|@devs> go").as_deref(), Some("@devs go"));
        assert_eq!(plain("<!subteam^S1> go").as_deref(), Some("@group go"));
        assert_eq!(plain("<@U999>"), None);
    }

    #[test]
    fn notification_text_flattens_links_and_html() {
        assert_eq!(
            plain("<https://x.io|site> <https://y.io> <mailto:a@b.io|mail>").as_deref(),
            Some("site (https://x.io) https://y.io mail")
        );
        assert_eq!(
            plain("<https://x.io/long/path|https://x.io/lo\u{2026}> <https://y.io/p|y.io/p...>")
                .as_deref(),
            Some("https://x.io/long/path https://y.io/p")
        );
        assert_eq!(
            plain("<a href=\"https://x.io\">label</a> <b>bold</b>").as_deref(),
            Some("label bold")
        );
        assert_eq!(plain("a<br>b").as_deref(), Some("a\nb"));
    }

    #[test]
    fn notification_text_decodes_entities() {
        assert_eq!(
            plain("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41;").as_deref(),
            Some("a & b <c> \"d\" 'e' A")
        );
        assert_eq!(
            plain("&lt;b&gt;x&lt;/b&gt; & 1 < 2").as_deref(),
            Some("<b>x</b> & 1 < 2")
        );
    }

    #[test]
    fn notification_text_converts_known_emoji_only() {
        assert_eq!(
            plain("ok :+1: :custom_blob: 12:30:45").as_deref(),
            Some("ok \u{1f44d} :custom_blob: 12:30:45")
        );
    }

    #[test]
    fn notification_text_strips_mrkdwn_markers() {
        assert_eq!(
            plain("*bold* _it_ ~gone~ `code` *_both_*").as_deref(),
            Some("bold it gone code both")
        );
        assert_eq!(
            plain("```\nlet x = 1;\n```\nafter").as_deref(),
            Some("let x = 1;\nafter")
        );
        assert_eq!(
            plain("2*3*4 snake_case_name a * b").as_deref(),
            Some("2*3*4 snake_case_name a * b")
        );
        assert_eq!(plain("*open <@U123>*").as_deref(), Some("open @Ada"));
    }

    #[test]
    fn notification_text_collapses_whitespace() {
        assert_eq!(
            plain("  a   b \n\n\n\n c \t d  ").as_deref(),
            Some("a b\n\nc d")
        );
        assert_eq!(plain("   ").as_deref(), Some(""));
    }

    #[test]
    fn message_uses_image_asset_matches_forwarded_quote_files_before_and_after_cache() {
        let preview_url = "https://files.slack.com/files-tmb/T1-FQUOTE/preview_480.png";
        let custom = HashMap::new();
        let catalog = EmojiCatalog::new(&custom);

        let raw = SlackMessage {
            ts: "1710000005.000100".to_string(),
            attachments: Some(vec![crate::models::SlackAttachment {
                is_share: Some(true),
                files: Some(vec![SlackFile {
                    id: Some("F_QUOTE".to_string()),
                    mimetype: Some("image/png".to_string()),
                    thumb_480: Some(preview_url.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(message_uses_image_asset(&raw, &catalog, preview_url));

        let cached = crate::slack_message_wire::normalize_cached_message(raw);
        assert!(cached.attachments.is_none());
        assert!(message_uses_image_asset(&cached, &catalog, preview_url));

        let poster_url = "https://files.slack.com/files-tmb/T1-FVID/poster.png";
        let fallback_thumb = "https://files.slack.com/files-tmb/T1-FVID/thumb_360.jpg";
        let motion_mp4 = "https://files.slack.com/files-tmb/T1-FVID/motion.mp4";
        let full_video = "https://files.slack.com/files-pri/T1-FVID/clip.mp4";
        let anim_gif = "https://files.slack.com/files-tmb/T1-FGIF/anim_480.gif";
        let gif_mp4 = "https://files.slack.com/files-tmb/T1-FGIF/gif.mp4";

        let vid_file = SlackFile {
            id: Some("F_VID".to_string()),
            mimetype: Some("video/mp4".to_string()),
            url_private: Some(full_video.to_string()),
            url_static_preview: Some(poster_url.to_string()),
            thumb_360: Some(fallback_thumb.to_string()),
            thumb_video: Some(motion_mp4.to_string()),
            ..Default::default()
        };
        let gif_file = SlackFile {
            id: Some("F_GIF".to_string()),
            mimetype: Some("image/gif".to_string()),
            thumb_480_gif: Some(anim_gif.to_string()),
            thumb_video: Some(gif_mp4.to_string()),
            ..Default::default()
        };
        assert_eq!(
            image_asset_request(&vid_file),
            Some((poster_url.to_string(), poster_url.to_string()))
        );
        assert_eq!(
            image_asset_request(&gif_file),
            Some((anim_gif.to_string(), anim_gif.to_string()))
        );

        let mixed = crate::slack_message_wire::normalize_cached_message(SlackMessage {
            ts: "1710000005.000200".to_string(),
            attachments: Some(vec![crate::models::SlackAttachment {
                is_share: Some(true),
                files: Some(vec![vid_file, gif_file]),
                ..Default::default()
            }]),
            ..Default::default()
        });
        assert!(message_uses_image_asset(&mixed, &catalog, poster_url));
        assert!(message_uses_image_asset(&mixed, &catalog, fallback_thumb));
        assert!(message_uses_image_asset(&mixed, &catalog, motion_mp4));
        assert!(!message_uses_image_asset(&mixed, &catalog, full_video));
        assert!(message_uses_image_asset(&mixed, &catalog, anim_gif));
        assert!(!message_uses_image_asset(&mixed, &catalog, gif_mp4));
    }
}
