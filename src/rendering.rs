use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::message_html::decode_html_entity_prefix;
use crate::models::SlackMessage;
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

fn extract_mentions(text: &str, ids: &mut Vec<String>) {
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

/// Converts Slack mrkdwn and stray HTML into plain text for desktop notifications.
///
/// Returns `None` while a mentioned user is still unresolved so callers can
/// wait for names. Custom workspace emoji stay as `:name:` text.
pub fn notification_plain_text(text: &str, user_names: &HashMap<String, String>) -> Option<String> {
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
                out.push_str(&plain_angle_token(&tail[..end], user_names)?);
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
fn plain_angle_token(inner: &str, user_names: &HashMap<String, String>) -> Option<String> {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(label.trim()).filter(|label| !label.is_empty())),
        None => (inner, None),
    };
    if let Some(user_id) = target.strip_prefix('@') {
        let name = user_names.get(user_id.trim())?.trim();
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
        return Some(label.unwrap_or(target).to_string());
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
            ("U123".to_string(), "Ada Lovelace".to_string()),
            ("U456".to_string(), "Grace Hopper".to_string()),
        ]);

        assert_eq!(
            notification_plain_text("Hi <@U123>, meet <@U456|grace>.", &names).as_deref(),
            Some("Hi @Ada Lovelace, meet @Grace Hopper.")
        );
        assert_eq!(notification_plain_text("Hi <@U999>", &names), None);
        assert_eq!(
            notification_plain_text("Malformed <@U123", &names).as_deref(),
            Some("Malformed <@U123")
        );
    }

    fn plain(text: &str) -> Option<String> {
        let names = HashMap::from([("U123".to_string(), "Ada".to_string())]);
        notification_plain_text(text, &names)
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
            Some("site https://y.io mail")
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
}
