use serde_json::Value;

use crate::models::{SlackAttachment, SlackFile};

use crate::rich_message::{
    MessageAccessory as RichAccessory, MessageAttachment as RichAttachment,
    MessageControl as RichControl, MessageControlConfirmation, MessageDocument as RichDocument,
    MessageField as RichField, MessageImage as RichImage, MessageLinkedText as RichLinkedText,
    MessageNode as RichNode, RichInline, RichInlineStyle, RichTextNode, SensitiveValue,
    SlackControlAction,
};

pub(crate) fn normalize_blocks_with_files(
    blocks: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
    files: &[SlackFile],
) -> RichDocument {
    normalize_blocks_with_callback_mode(
        blocks,
        choose_option_label,
        more_actions_label,
        files,
        true,
    )
}

fn normalize_blocks_with_callback_mode(
    blocks: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
    files: &[SlackFile],
    retain_callbacks: bool,
) -> RichDocument {
    let nodes = blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            normalize_block(
                block,
                choose_option_label,
                more_actions_label,
                files,
                retain_callbacks,
            )
        })
        .collect();
    RichDocument::new(nodes, None)
}

fn normalize_block(
    block: &Value,
    choose_option_label: &str,
    more_actions_label: &str,
    files: &[SlackFile],
    retain_callbacks: bool,
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
                normalize_accessory(
                    accessory,
                    block.get("block_id").and_then(Value::as_str),
                    choose_option_label,
                    more_actions_label,
                    files,
                    retain_callbacks,
                )
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
        "image" => normalize_image(block, files).map(RichNode::Image),
        "actions" => {
            let controls = block
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|element| {
                    normalize_control(
                        element,
                        block.get("block_id").and_then(Value::as_str),
                        choose_option_label,
                        more_actions_label,
                        retain_callbacks,
                    )
                })
                .collect::<Vec<_>>();
            (!controls.is_empty()).then_some(RichNode::Actions(controls))
        }
        "call" => normalize_call_block(block),
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
    block_id: Option<&str>,
    choose_option_label: &str,
    more_actions_label: &str,
    files: &[SlackFile],
    retain_callbacks: bool,
) -> Option<RichAccessory> {
    if value.get("type").and_then(Value::as_str) == Some("image") {
        return normalize_image(value, files).map(RichAccessory::Image);
    }
    normalize_control(
        value,
        block_id,
        choose_option_label,
        more_actions_label,
        retain_callbacks,
    )
    .map(RichAccessory::Control)
}

fn normalize_image(value: &Value, files: &[SlackFile]) -> Option<RichImage> {
    let alt = value
        .get("alt_text")
        .and_then(Value::as_str)
        .unwrap_or("Image")
        .to_string();
    let url = value
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("slack_file")
                .and_then(|file| file.get("url"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            let file_id = value
                .get("slack_file")
                .and_then(|file| file.get("id"))
                .and_then(Value::as_str)?;
            files
                .iter()
                .find(|file| file.id.as_deref() == Some(file_id))
                .and_then(SlackFile::preview_url)
        })
        .map(ToString::to_string);
    let title = value
        .get("title")
        .and_then(block_text)
        .filter(|title| !title.trim().is_empty());
    (url.is_some() || !alt.trim().is_empty()).then_some(RichImage { url, alt, title })
}

fn normalize_call_block(block: &Value) -> Option<RichNode> {
    let call_val = block.get("call");
    let v1_val = call_val.and_then(|c| c.get("v1"));

    let join_url = v1_val
        .and_then(|v1| v1.get("join_url"))
        .and_then(Value::as_str)
        .or_else(|| call_val.and_then(|c| c.get("join_url")).and_then(Value::as_str))
        .or_else(|| v1_val.and_then(|v1| v1.get("desktop_app_join_url")).and_then(Value::as_str))
        .or_else(|| call_val.and_then(|c| c.get("desktop_app_join_url")).and_then(Value::as_str))
        .or_else(|| block.get("join_url").and_then(Value::as_str))
        .or_else(|| block.get("url").and_then(Value::as_str));

    let name = v1_val
        .and_then(|v1| v1.get("name"))
        .and_then(Value::as_str)
        .or_else(|| call_val.and_then(|c| c.get("name")).and_then(Value::as_str))
        .or_else(|| block.get("name").and_then(Value::as_str))
        .filter(|n| !n.trim().is_empty());

    let is_teams = name.is_some_and(|n| n.to_lowercase().contains("teams"))
        || call_val.and_then(|c| c.get("media_backend_type")).and_then(Value::as_str) == Some("msteams")
        || join_url.is_some_and(|u| u.contains("teams.microsoft.com") || u.starts_with("msteams:"));

    let default_title = if is_teams { "Microsoft Teams Meeting" } else { "Call" };
    let title = name.unwrap_or(default_title);

    let Some(url) = join_url else {
        return Some(RichNode::Header(title.to_string()));
    };

    Some(RichNode::Section {
        text: Some(format!("*{title}*")),
        fields: Vec::new(),
        accessory: Some(RichAccessory::Control(RichControl::presentation(
            "Join".to_string(),
            Some(url.to_string()),
            false,
        ))),
    })
}

fn normalize_control(
    value: &Value,
    block_id: Option<&str>,
    choose_option_label: &str,
    more_actions_label: &str,
    retain_callbacks: bool,
) -> Option<RichControl> {
    let kind = value.get("type")?.as_str()?;
    let url = extract_control_url(value);
    let label = match kind {
        "button" => control_label(value, url.as_deref()),
        "static_select" | "multi_static_select" => value
            .get("placeholder")
            .and_then(block_text)
            .unwrap_or_else(|| choose_option_label.to_string()),
        "overflow" => more_actions_label.to_string(),
        _ => return None,
    };
    if kind == "button"
        && retain_callbacks
        && url.is_none()
        && value
            .get("action_id")
            .and_then(Value::as_str)
            .is_some_and(|action_id| !action_id.trim().is_empty())
        && block_id.is_some_and(|block_id| !block_id.trim().is_empty())
    {
        let mut action = value.clone();
        if let Some(action) = action.as_object_mut() {
            action.retain(|key, value| {
                matches!(
                    key.as_str(),
                    "type" | "action_id" | "text" | "value" | "style" | "third_party_auth"
                ) && !value.is_null()
            });
            action.insert(
                "block_id".to_string(),
                Value::String(block_id.unwrap_or_default().to_string()),
            );
        }
        if let Ok(action) = serde_json::to_string(&action) {
            return Some(RichControl::callback(
                label,
                SlackControlAction::Block {
                    action: SensitiveValue::new(action),
                },
                normalize_block_confirmation(value.get("confirm")),
            ));
        }
    }
    Some(RichControl::presentation(
        label,
        url,
        value.get("confirm").is_some(),
    ))
}

fn control_label(value: &Value, url: Option<&str>) -> String {
    block_text(value)
        .or_else(|| value.get("label").and_then(block_text))
        .or_else(|| value.get("label").and_then(Value::as_str).map(ToString::to_string))
        .or_else(|| value.get("name").and_then(Value::as_str).map(ToString::to_string))
        .or_else(|| {
            value
                .get("value")
                .and_then(Value::as_str)
                .filter(|v| !v.starts_with("http://") && !v.starts_with("https://") && !v.starts_with("msteams:"))
                .map(ToString::to_string)
        })
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            if let Some(url) = url {
                if url.contains("teams.microsoft.com") || url.starts_with("msteams:") || url.contains("zoom.us") {
                    "Join".to_string()
                } else {
                    "Open".to_string()
                }
            } else {
                "Action".to_string()
            }
        })
}

fn extract_control_url(value: &Value) -> Option<String> {
    value
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| value.get("action_url").and_then(Value::as_str))
        .or_else(|| value.get("join_url").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("value")
                .and_then(Value::as_str)
                .filter(|v| v.starts_with("http://") || v.starts_with("https://") || v.starts_with("msteams:"))
        })
        .map(ToString::to_string)
}

fn normalize_block_confirmation(value: Option<&Value>) -> Option<MessageControlConfirmation> {
    let value = value?;
    Some(MessageControlConfirmation {
        title: value.get("title").and_then(block_text),
        text: value.get("text").and_then(block_text),
        confirm_label: value.get("confirm").and_then(block_text),
        deny_label: value.get("deny").and_then(block_text),
        destructive: value.get("style").and_then(Value::as_str) == Some("danger"),
    })
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
        underline: style_flag(value, "underline"),
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

pub(crate) fn normalize_attachments(
    attachments: &[SlackAttachment],
    files: &[SlackFile],
) -> RichDocument {
    let mut nodes = Vec::new();
    for (attachment_index, attachment) in attachments.iter().enumerate() {
        if let Some(blocks) = attachment.blocks.as_ref() {
            let embedded = normalize_blocks_with_callback_mode(
                blocks,
                "Choose an option",
                "More actions",
                files,
                false,
            )
            .nodes;
            if !embedded.is_empty() {
                nodes.extend(embedded);
                continue;
            }
        }
        if let Some(attachment) = normalize_attachment(attachment, attachment_index) {
            nodes.push(RichNode::Attachment(Box::new(attachment)));
        }
    }
    RichDocument::new(nodes, None)
}

fn normalize_attachment(
    attachment: &SlackAttachment,
    attachment_index: usize,
) -> Option<RichAttachment> {
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
            let label = action.label()?;
            if action.url.is_none()
                && action.kind.as_deref() == Some("button")
                && attachment
                    .callback_id
                    .as_deref()
                    .is_some_and(|callback_id| !callback_id.trim().is_empty())
            {
                let mut selected_action = serde_json::to_value(action).ok()?;
                if let Some(selected_action) = selected_action.as_object_mut() {
                    selected_action.retain(|key, value| {
                        matches!(
                            key.as_str(),
                            "id" | "name" | "text" | "type" | "style" | "value"
                        ) && !value.is_null()
                    });
                }
                return Some(RichControl::callback(
                    label,
                    SlackControlAction::LegacyAttachment {
                        attachment_id: attachment
                            .id
                            .unwrap_or_else(|| attachment_index.saturating_add(1) as u64),
                        callback_id: SensitiveValue::new(
                            attachment.callback_id.clone().unwrap_or_default(),
                        ),
                        action: SensitiveValue::new(serde_json::to_string(&selected_action).ok()?),
                    },
                    action
                        .confirm
                        .as_ref()
                        .map(|confirmation| MessageControlConfirmation {
                            title: confirmation.title.clone(),
                            text: confirmation.text.clone(),
                            confirm_label: confirmation.ok_text.clone(),
                            deny_label: confirmation.dismiss_text.clone(),
                            destructive: action.style.as_deref() == Some("danger"),
                        }),
                ));
            }
            Some(RichControl::presentation(
                label,
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
        let document = normalize_blocks_with_files(
            message.blocks.as_ref().unwrap(),
            "Choose",
            "More actions",
            &[],
        );

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
        let document = normalize_attachments(message.attachments.as_deref().unwrap(), &[]);
        let debug = format!("{document:?}");

        assert!(debug.contains("#2eb67d"));
        assert!(debug.contains("Approve"));
        assert!(!debug.contains("private-callback"));
        assert!(!debug.contains("private-approval-value"));

        let [RichNode::Attachment(attachment)] = document.nodes() else {
            panic!("expected one attachment");
        };
        let [control] = attachment.actions.as_slice() else {
            panic!("expected one attachment action");
        };
        let Some(SlackControlAction::LegacyAttachment {
            attachment_id,
            callback_id,
            action,
        }) = control.action()
        else {
            panic!("expected retained legacy callback");
        };
        assert_eq!(*attachment_id, 1);
        assert_eq!(callback_id.expose(), "private-callback");
        assert!(action.expose().contains("private-approval-value"));
        let cached = serde_json::to_string(&document).expect("canonical document serializes");
        assert!(!cached.contains("private-callback"));
        assert!(!cached.contains("private-approval-value"));
    }

    #[test]
    fn retains_block_button_callback_in_memory_only() {
        let document = normalize_blocks_with_files(
            &serde_json::json!([{
                "type": "actions",
                "block_id": "private-block",
                "elements": [{
                    "type": "button",
                    "action_id": "private-action",
                    "value": "private-value",
                    "text": {"type": "plain_text", "text": "Approve"},
                    "confirm": {
                        "title": {"type": "plain_text", "text": "Approve request?"},
                        "text": {"type": "mrkdwn", "text": "This cannot be undone."},
                        "confirm": {"type": "plain_text", "text": "Approve"},
                        "deny": {"type": "plain_text", "text": "Cancel"}
                    }
                }]
            }]),
            "Choose",
            "More actions",
            &[],
        );
        let [RichNode::Actions(actions)] = document.nodes() else {
            panic!("expected one actions block");
        };
        let [control] = actions.as_slice() else {
            panic!("expected one button");
        };
        let Some(SlackControlAction::Block { action }) = control.action() else {
            panic!("expected retained Block Kit callback");
        };
        assert!(action.expose().contains("private-action"));
        assert!(!action.expose().contains("confirm"));
        assert_eq!(
            control
                .confirmation
                .as_ref()
                .and_then(|value| value.title.as_deref()),
            Some("Approve request?")
        );
        let debug = format!("{document:?}");
        let cached = serde_json::to_string(&document).expect("canonical document serializes");
        for sensitive in ["private-block", "private-action", "private-value"] {
            assert!(!debug.contains(sensitive));
            assert!(!cached.contains(sensitive));
        }
    }

    #[test]
    fn normalizes_slack_file_urls_for_image_blocks_and_accessories() {
        let document = normalize_blocks_with_files(
            &serde_json::json!([
                {
                    "type": "image",
                    "slack_file": {
                        "url": "https://files.slack.com/files-pri/F1/animated.gif"
                    },
                    "alt_text": "shared a GIF"
                },
                {
                    "type": "section",
                    "text": {"type": "mrkdwn", "text": "Reaction"},
                    "accessory": {
                        "type": "image",
                        "slack_file": {
                            "url": "https://files.slack.com/files-pri/F2/accessory.gif"
                        },
                        "alt_text": "animated reaction"
                    }
                }
            ]),
            "Choose",
            "More actions",
            &[],
        );

        let [RichNode::Image(image), RichNode::Section { accessory, .. }] = document.nodes() else {
            panic!("expected image block and section");
        };
        assert_eq!(
            image.url.as_deref(),
            Some("https://files.slack.com/files-pri/F1/animated.gif")
        );
        let Some(RichAccessory::Image(accessory)) = accessory else {
            panic!("expected image accessory");
        };
        assert_eq!(
            accessory.url.as_deref(),
            Some("https://files.slack.com/files-pri/F2/accessory.gif")
        );
    }

    #[test]
    fn normalizes_call_blocks_with_join_button() {
        let document = normalize_blocks_with_files(
            &serde_json::json!([
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": "A new call was started by Slack Teams Calls"
                    }
                },
                {
                    "type": "call",
                    "call_id": "R01234567",
                    "call": {
                        "media_backend_type": "msteams",
                        "v1": {
                            "id": "R01234567",
                            "name": "Microsoft Teams Meeting",
                            "join_url": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc"
                        }
                    }
                }
            ]),
            "Choose",
            "More actions",
            &[],
        );

        let [RichNode::Section { text: text1, .. }, RichNode::Section { text: text2, accessory, .. }] = document.nodes() else {
            panic!("expected two sections");
        };
        assert_eq!(text1.as_deref(), Some("A new call was started by Slack Teams Calls"));
        assert_eq!(text2.as_deref(), Some("*Microsoft Teams Meeting*"));
        let Some(RichAccessory::Control(control)) = accessory else {
            panic!("expected control accessory");
        };
        assert_eq!(control.label(), "Join");
        assert_eq!(
            control.url(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc")
        );
    }
}
