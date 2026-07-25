use serde_json::Value;

use crate::models::SlackAttachment;

use crate::rich_message::{
    MessageAccessory as RichAccessory, MessageAttachment as RichAttachment,
    MessageControl as RichControl, MessageDocument as RichDocument, MessageField as RichField,
    MessageImage as RichImage, MessageLinkedText as RichLinkedText, MessageNode as RichNode,
    RichInline, RichInlineStyle, RichTextNode,
};

pub(crate) fn normalize_blocks(
    blocks: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
) -> RichDocument {
    let nodes = blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| normalize_block(block, choose_option_label, more_actions_label))
        .collect();
    RichDocument::new(nodes, None)
}

fn normalize_block(
    block: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
) -> Option<RichNode> {
    match block.get("type")?.as_str()? {
        "header" => block_text(block).map(RichNode::Header),
        "section" => {
            let text = block_text(block);
            let fields = block
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(block_text)
                .collect::<Vec<_>>();
            let accessory = block.get("accessory").and_then(|accessory| {
                normalize_accessory(accessory, choose_option_label, more_actions_label)
            });
            (text.is_some() || !fields.is_empty() || accessory.is_some()).then_some(
                RichNode::Section {
                    text,
                    fields,
                    accessory,
                },
            )
        }
        "context" => {
            let elements = block
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(block_text)
                .collect::<Vec<_>>();
            (!elements.is_empty()).then_some(RichNode::Context(elements))
        }
        "divider" => Some(RichNode::Divider),
        "image" => normalize_image(block).map(RichNode::Image),
        "actions" => {
            let controls = block
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|element| {
                    normalize_control(element, choose_option_label, more_actions_label)
                })
                .collect::<Vec<_>>();
            (!controls.is_empty()).then_some(RichNode::Actions(controls))
        }
        "rich_text" => {
            let nodes = block
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(normalize_rich_text_node)
                .collect::<Vec<_>>();
            (!nodes.is_empty()).then_some(RichNode::RichText(nodes))
        }
        _ => None,
    }
}

fn normalize_accessory(
    value: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
) -> Option<RichAccessory> {
    if value.get("type").and_then(Value::as_str) == Some("image") {
        return normalize_image(value).map(RichAccessory::Image);
    }
    normalize_control(value, choose_option_label, more_actions_label).map(RichAccessory::Control)
}

fn normalize_image(value: &Value) -> Option<RichImage> {
    let alt = value
        .get("alt_text")
        .and_then(Value::as_str)
        .unwrap_or("Image")
        .to_string();
    let url = value
        .get("image_url")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let title = value
        .get("title")
        .and_then(block_text)
        .filter(|title| !title.trim().is_empty());
    (url.is_some() || !alt.trim().is_empty()).then_some(RichImage { url, alt, title })
}

fn normalize_control(
    value: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
) -> Option<RichControl> {
    let label = match value.get("type")?.as_str()? {
        "button" => block_text(value)?,
        "static_select" | "multi_static_select" => value
            .get("placeholder")
            .and_then(block_text)
            .unwrap_or_else(|| choose_option_label.to_string()),
        "overflow" => more_actions_label.to_string(),
        _ => return None,
    };
    Some(RichControl::presentation(
        label,
        value
            .get("url")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        value.get("confirm").is_some(),
    ))
}

fn normalize_rich_text_node(value: &Value) -> Option<RichTextNode> {
    match value.get("type")?.as_str()? {
        "rich_text_section" => non_empty_inlines(value).map(RichTextNode::Paragraph),
        "rich_text_preformatted" => non_empty_inlines(value).map(RichTextNode::Preformatted),
        "rich_text_quote" => non_empty_inlines(value).map(RichTextNode::Quote),
        "rich_text_list" => {
            let items = value
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(non_empty_inlines)
                .collect::<Vec<_>>();
            (!items.is_empty()).then_some(RichTextNode::List {
                ordered: value.get("style").and_then(Value::as_str) == Some("ordered"),
                items,
            })
        }
        _ => None,
    }
}

fn non_empty_inlines(value: &Value) -> Option<Vec<RichInline>> {
    let inlines = value
        .get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(normalize_inline)
        .collect::<Vec<_>>();
    (!inlines.is_empty()).then_some(inlines)
}

fn normalize_inline(value: &Value) -> Option<RichInline> {
    let style = normalize_inline_style(value.get("style"));
    match value.get("type")?.as_str()? {
        "text" => Some(RichInline::Text {
            text: value.get("text")?.as_str()?.to_string(),
            style,
        }),
        "link" => {
            let url = value.get("url")?.as_str()?.to_string();
            Some(RichInline::Link {
                label: value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or(&url)
                    .to_string(),
                url,
                style,
            })
        }
        "user" => Some(RichInline::User(
            value.get("user_id")?.as_str()?.to_string(),
        )),
        "channel" => Some(RichInline::Channel(
            value.get("channel_id")?.as_str()?.to_string(),
        )),
        "emoji" => Some(RichInline::Emoji(value.get("name")?.as_str()?.to_string())),
        _ => None,
    }
}

fn normalize_inline_style(value: Option<&Value>) -> RichInlineStyle {
    RichInlineStyle {
        bold: style_flag(value, "bold"),
        italic: style_flag(value, "italic"),
        strike: style_flag(value, "strike"),
        code: style_flag(value, "code"),
    }
}

fn style_flag(value: Option<&Value>, name: &str) -> bool {
    value
        .and_then(|value| value.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn block_text(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(|text| {
            text.as_str()
                .or_else(|| text.get("text").and_then(Value::as_str))
        })
        .map(ToString::to_string)
}

pub(crate) fn normalize_attachments(attachments: &[SlackAttachment]) -> RichDocument {
    let nodes = attachments
        .iter()
        .filter_map(normalize_attachment)
        .map(Box::new)
        .map(RichNode::Attachment)
        .collect();
    RichDocument::new(nodes, None)
}

fn normalize_attachment(attachment: &SlackAttachment) -> Option<RichAttachment> {
    if !attachment.has_visible_content() {
        return None;
    }
    let author = linked_text(
        attachment.author_name.as_deref(),
        attachment.author_link.as_deref(),
    );
    let title = linked_text(
        attachment.title.as_deref(),
        attachment.title_link.as_deref(),
    );
    let fields = attachment
        .fields
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|field| {
            let title = non_empty(field.title.as_deref()).map(ToString::to_string);
            let value = non_empty(field.value.as_deref()).map(ToString::to_string);
            (title.is_some() || value.is_some()).then_some(RichField {
                title,
                value,
                short: field.short.unwrap_or(false),
            })
        })
        .collect();
    let image_url = non_empty(attachment.image_url.as_deref())
        .or_else(|| non_empty(attachment.thumb_url.as_deref()))
        .map(ToString::to_string);
    let image = image_url.map(|url| RichImage {
        url: Some(url),
        alt: non_empty(attachment.title.as_deref())
            .or_else(|| non_empty(attachment.fallback.as_deref()))
            .unwrap_or("Attachment image")
            .to_string(),
        title: attachment.title.clone(),
    });
    let actions = attachment
        .actions
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|action| {
            Some(RichControl::presentation(
                action.label()?,
                action.url.clone(),
                action.confirm.is_some(),
            ))
        })
        .collect();
    Some(RichAttachment {
        color: attachment.color.as_deref().and_then(normalize_color),
        pretext: non_empty(attachment.pretext.as_deref()).map(ToString::to_string),
        author,
        title,
        text: non_empty(attachment.text.as_deref()).map(ToString::to_string),
        fallback: non_empty(attachment.fallback.as_deref()).map(ToString::to_string),
        fields,
        image,
        actions,
        footer: non_empty(attachment.footer.as_deref()).map(ToString::to_string),
    })
}

fn linked_text(text: Option<&str>, url: Option<&str>) -> Option<RichLinkedText> {
    Some(RichLinkedText {
        text: non_empty(text)?.to_string(),
        url: non_empty(url).map(ToString::to_string),
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalize_color(value: &str) -> Option<String> {
    match value.trim() {
        "good" => Some("#2eb67d".to_string()),
        "warning" => Some("#ecb22e".to_string()),
        "danger" => Some("#e01e5a".to_string()),
        value
            if value.starts_with('#')
                && matches!(value.len(), 4 | 7)
                && value[1..]
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()) =>
        {
            Some(value.to_ascii_lowercase())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_html::test_fixtures;

    #[test]
    fn normalizes_valid_jira_siblings_without_sensitive_control_data() {
        let message = test_fixtures::jira_message();
        let document = normalize_blocks(message.blocks.as_ref().unwrap(), "Choose", "More actions");

        assert_eq!(document.nodes.len(), 4);
        let debug = format!("{document:?}");
        for sensitive in [
            "private-action",
            "private-value",
            "private-select",
            "private-option",
            "private-overflow",
            "private-delete",
            "ignored-sibling-value",
        ] {
            assert!(!debug.contains(sensitive), "retained {sensitive}");
        }
        assert!(debug.contains("Issue icon"));
        assert!(debug.contains("Open issue"));
        assert!(debug.contains("Set status"));
    }

    #[test]
    fn normalizes_attachment_color_and_drops_callback_metadata() {
        let message = test_fixtures::bob_message();
        let document = normalize_attachments(message.attachments.as_deref().unwrap());
        let debug = format!("{document:?}");

        assert!(debug.contains("#2eb67d"));
        assert!(debug.contains("Approve"));
        assert!(!debug.contains("private-callback"));
        assert!(!debug.contains("private-approval-value"));
    }
}
