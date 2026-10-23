//! Emoji in message text: `:shortcode:` parsing, inline Pango rendering, and
//! the emoji-only ("jumbo") message check.
//!
//! Sizes follow Slack: emoji inside a sentence render at
//! [`INLINE_EMOJI_SIZE`], while a message made of nothing but emoji renders
//! them at the reaction size.

use crate::emoji::{EmojiCatalog, EmojiValue};

use super::{escape_pango, MessageRenderContext};

/// Pixel size of an emoji inside message text, unicode and custom alike.
pub const INLINE_EMOJI_SIZE: i32 = 22;

/// Most emoji a message may hold and still render jumbo, as in Slack.
pub const JUMBO_EMOJI_LIMIT: usize = 23;

/// Renders a leading `:shortcode:` (with an optional adjacent skin tone) as
/// Pango. Returns the markup and the bytes consumed, or `None` when `text`
/// does not start with a well-formed shortcode.
pub(super) fn render_emoji_shortcode_pango(
    text: &str,
    context: &MessageRenderContext,
) -> Option<(String, usize)> {
    let (resolved_code, consumed) = parse_shortcode(text)?;
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let rendered = match catalog.resolve(&resolved_code) {
        Some(EmojiValue::Unicode(val)) => format!(
            "<span font_desc=\"{INLINE_EMOJI_SIZE}px\">{}</span>",
            escape_pango(val)
        ),
        Some(EmojiValue::CustomImage(ref url)) => format!(
            "<conduit-custom-emoji name=\"{}\" url=\"{}\"/>",
            escape_pango(&resolved_code),
            escape_pango(url)
        ),
        None => escape_pango(&text[..consumed]),
    };
    Some((rendered, consumed))
}

/// Emoji names when `text` is only emoji and whitespace, at most
/// [`JUMBO_EMOJI_LIMIT`] of them; `None` for anything else. Unicode emoji are
/// returned as their shortcode so callers resolve both kinds the same way.
pub fn emoji_only_names(text: &str, context: &MessageRenderContext) -> Option<Vec<String>> {
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let mut names = Vec::new();
    for word in text.split_whitespace() {
        if let Some(emoji) = emojis::get(word) {
            names.push(emoji.shortcode()?.to_string());
            continue;
        }
        let mut rest = word;
        while !rest.is_empty() {
            let (code, consumed) = parse_shortcode(rest)?;
            catalog.resolve(&code)?;
            names.push(code);
            rest = &rest[consumed..];
        }
    }
    (!names.is_empty() && names.len() <= JUMBO_EMOJI_LIMIT).then_some(names)
}

/// Parses a leading `:code:` plus an adjacent `:skin-tone-N:`, returning the
/// catalog lookup name (`code::skin-tone-N`) and the bytes consumed.
fn parse_shortcode(text: &str) -> Option<(String, usize)> {
    let (code, mut consumed) = shortcode_token(text)?;
    let mut resolved_code = code.to_string();
    if let Some((modifier, modifier_len)) = shortcode_token(&text[consumed..])
        .filter(|(modifier, _)| modifier.starts_with("skin-tone-"))
    {
        resolved_code.push_str("::");
        resolved_code.push_str(modifier);
        consumed += modifier_len;
    }
    Some((resolved_code, consumed))
}

/// A leading `:code:` token: its code and the bytes it spans.
fn shortcode_token(text: &str) -> Option<(&str, usize)> {
    let body = text.strip_prefix(':')?;
    let end = body.find(':')?;
    let code = &body[..end];
    let valid = !code.is_empty()
        && code.len() <= 64
        && code.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        });
    valid.then_some((code, end + 2))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    fn context_with_custom(name: &str) -> MessageRenderContext {
        MessageRenderContext {
            custom_emojis: Arc::new(HashMap::from([(
                name.to_string(),
                "https://emoji.slack-edge.com/x.png".to_string(),
            )])),
            ..Default::default()
        }
    }

    #[test]
    fn unicode_shortcode_renders_at_inline_size() {
        let context = MessageRenderContext::default();
        let (pango, consumed) = render_emoji_shortcode_pango(":smile: hi", &context).unwrap();
        assert_eq!(pango, "<span font_desc=\"22px\">😄</span>");
        assert_eq!(consumed, ":smile:".len());
    }

    #[test]
    fn skin_tone_is_consumed_with_its_base() {
        let context = MessageRenderContext::default();
        let (_, consumed) = render_emoji_shortcode_pango(":+1::skin-tone-3: ok", &context).unwrap();
        assert_eq!(consumed, ":+1::skin-tone-3:".len());
    }

    #[test]
    fn unknown_shortcode_stays_literal() {
        let context = MessageRenderContext::default();
        let (pango, _) = render_emoji_shortcode_pango(":nope_nope:", &context).unwrap();
        assert_eq!(pango, ":nope_nope:");
    }

    #[test]
    fn emoji_only_accepts_shortcodes_unicode_and_custom() {
        let context = context_with_custom("partyparrot");
        assert_eq!(
            emoji_only_names(" :smile::partyparrot: 👍 :+1::skin-tone-2:\n", &context),
            Some(vec![
                "smile".to_string(),
                "partyparrot".to_string(),
                "+1".to_string(),
                "+1::skin-tone-2".to_string(),
            ])
        );
    }

    #[test]
    fn emoji_only_rejects_text_unknown_codes_and_overflow() {
        let context = MessageRenderContext::default();
        assert_eq!(emoji_only_names(":smile: hi", &context), None);
        assert_eq!(emoji_only_names(":not_a_real_emoji:", &context), None);
        assert_eq!(emoji_only_names("   ", &context), None);
        let too_many = ":smile: ".repeat(JUMBO_EMOJI_LIMIT + 1);
        assert_eq!(emoji_only_names(&too_many, &context), None);
        let at_limit = ":smile: ".repeat(JUMBO_EMOJI_LIMIT);
        assert_eq!(
            emoji_only_names(&at_limit, &context).map(|names| names.len()),
            Some(JUMBO_EMOJI_LIMIT)
        );
    }
}
