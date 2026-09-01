use std::fmt;

use serde::{Deserialize, Serialize};

pub const MESSAGE_CONTENT_VERSION: u16 = 1;

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SensitiveValue(String);

impl SensitiveValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageControlKey(u32);

#[derive(Clone, PartialEq, Eq)]
pub enum SlackControlAction {
    Block {
        action: SensitiveValue,
    },
    LegacyAttachment {
        attachment_id: u64,
        callback_id: SensitiveValue,
        action: SensitiveValue,
    },
}

impl fmt::Debug for SlackControlAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block { .. } => formatter.write_str("SlackControlAction::Block([REDACTED])"),
            Self::LegacyAttachment { .. } => {
                formatter.write_str("SlackControlAction::LegacyAttachment([REDACTED])")
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageControlConfirmation {
    pub(crate) title: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) confirm_label: Option<String>,
    pub(crate) deny_label: Option<String>,
    pub(crate) destructive: bool,
}

impl fmt::Debug for SensitiveValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveValue([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageAuthor {
    User {
        user_id: String,
    },
    App {
        app_id: Option<String>,
        bot_id: Option<String>,
        display_name: String,
        avatar_url: Option<String>,
        icon_emoji: Option<String>,
    },
    Unknown {
        display_name: String,
    },
}

impl Default for MessageAuthor {
    fn default() -> Self {
        Self::Unknown {
            display_name: "Slack".to_string(),
        }
    }
}

impl MessageAuthor {
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Self::User { user_id } => Some(user_id),
            Self::App { .. } | Self::Unknown { .. } => None,
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::User { user_id } => user_id,
            Self::App { display_name, .. } | Self::Unknown { display_name } => display_name,
        }
    }

    pub fn avatar_url(&self) -> Option<&str> {
        match self {
            Self::App { avatar_url, .. } => avatar_url.as_deref(),
            Self::User { .. } | Self::Unknown { .. } => None,
        }
    }

    pub fn icon_emoji(&self) -> Option<&str> {
        match self {
            Self::App { icon_emoji, .. } => icon_emoji.as_deref(),
            Self::User { .. } | Self::Unknown { .. } => None,
        }
    }

    pub(crate) fn app_id(&self) -> Option<&str> {
        match self {
            Self::App { app_id, .. } => app_id.as_deref(),
            Self::User { .. } | Self::Unknown { .. } => None,
        }
    }

    pub(crate) fn bot_id(&self) -> Option<&str> {
        match self {
            Self::App { bot_id, .. } => bot_id.as_deref(),
            Self::User { .. } | Self::Unknown { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageControl {
    #[serde(default)]
    pub(crate) key: Option<MessageControlKey>,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) confirmation_required: bool,
    #[serde(default)]
    value: Option<SensitiveValue>,
    #[serde(skip)]
    action: Option<SlackControlAction>,
    #[serde(default)]
    pub(crate) confirmation: Option<MessageControlConfirmation>,
}

impl MessageControl {
    pub fn new(label: impl Into<String>, value: Option<SensitiveValue>) -> Self {
        Self {
            key: None,
            label: label.into(),
            url: None,
            confirmation_required: false,
            value,
            action: None,
            confirmation: None,
        }
    }

    pub(crate) fn presentation(
        label: impl Into<String>,
        url: Option<String>,
        confirmation_required: bool,
    ) -> Self {
        Self {
            key: None,
            label: label.into(),
            url,
            confirmation_required,
            value: None,
            action: None,
            confirmation: None,
        }
    }

    pub(crate) fn callback(
        label: impl Into<String>,
        action: SlackControlAction,
        confirmation: Option<MessageControlConfirmation>,
    ) -> Self {
        Self {
            key: None,
            label: label.into(),
            url: None,
            confirmation_required: confirmation.is_some(),
            value: None,
            action: Some(action),
            confirmation,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub(crate) fn key(&self) -> Option<MessageControlKey> {
        self.key
    }

    pub(crate) fn action(&self) -> Option<&SlackControlAction> {
        self.action.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageNode {
    Text(String),
    Control(MessageControl),
    Image(MessageImage),
    Header(String),
    Section {
        text: Option<String>,
        fields: Vec<String>,
        accessory: Option<MessageAccessory>,
    },
    Context(Vec<String>),
    Divider,
    Actions(Vec<MessageControl>),
    RichText(Vec<RichTextNode>),
    Attachment(Box<MessageAttachment>),
    Unsupported {
        type_label: String,
        fallback: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageAccessory {
    Control(MessageControl),
    Image(MessageImage),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageImage {
    pub(crate) url: Option<String>,
    pub(crate) alt: String,
    pub(crate) title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichTextNode {
    Paragraph(Vec<RichInline>),
    Preformatted(Vec<RichInline>),
    Quote(Vec<RichInline>),
    List {
        ordered: bool,
        items: Vec<Vec<RichInline>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichInline {
    Text {
        text: String,
        style: RichInlineStyle,
    },
    Link {
        url: String,
        label: String,
        style: RichInlineStyle,
    },
    User(String),
    Channel(String),
    Emoji(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichInlineStyle {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    #[serde(default)]
    pub(crate) underline: bool,
    pub(crate) strike: bool,
    pub(crate) code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttachment {
    pub(crate) color: Option<String>,
    pub(crate) pretext: Option<String>,
    pub(crate) author: Option<MessageLinkedText>,
    pub(crate) title: Option<MessageLinkedText>,
    pub(crate) text: Option<String>,
    pub(crate) fallback: Option<String>,
    pub(crate) fields: Vec<MessageField>,
    pub(crate) image: Option<MessageImage>,
    pub(crate) actions: Vec<MessageControl>,
    pub(crate) footer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageLinkedText {
    pub(crate) text: String,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageField {
    pub(crate) title: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) short: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDocument {
    pub(crate) nodes: Vec<MessageNode>,
    accessible_fallback: Option<String>,
}

impl MessageDocument {
    pub fn new(mut nodes: Vec<MessageNode>, accessible_fallback: Option<String>) -> Self {
        assign_control_keys(&mut nodes);
        Self {
            nodes,
            accessible_fallback: accessible_fallback.filter(|text| !text.trim().is_empty()),
        }
    }

    pub fn nodes(&self) -> &[MessageNode] {
        &self.nodes
    }

    pub fn image_urls(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().filter_map(|node| {
            let image = match node {
                MessageNode::Image(image) => Some(image),
                MessageNode::Section {
                    accessory: Some(MessageAccessory::Image(image)),
                    ..
                } => Some(image),
                MessageNode::Attachment(attachment) => attachment.image.as_ref(),
                _ => None,
            }?;
            image.url.as_deref().and_then(non_empty)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn has_visible_content(&self) -> bool {
        !self.nodes.is_empty() || self.accessible_fallback.is_some()
    }

    pub fn visible_text(&self) -> String {
        self.project_text(false)
    }

    pub fn accessible_text(&self) -> String {
        self.project_text(true)
    }

    fn project_text(&self, include_controls: bool) -> String {
        let mut parts = Vec::new();
        for node in &self.nodes {
            project_node_text(node, include_controls, &mut parts);
        }
        if parts.is_empty() {
            parts.extend(self.accessible_fallback.iter().cloned());
        }
        parts.join("\n")
    }

    pub fn mentioned_user_ids(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().flat_map(message_node_user_ids)
    }

    pub(crate) fn control_keys(&self) -> Vec<MessageControlKey> {
        let mut keys = Vec::new();
        for node in &self.nodes {
            visit_node_controls(node, &mut |control| {
                if let Some(key) = control.key() {
                    keys.push(key);
                }
            });
        }
        keys
    }

    pub(crate) fn control(&self, key: MessageControlKey) -> Option<&MessageControl> {
        for node in &self.nodes {
            let mut found = None;
            visit_node_controls(node, &mut |control| {
                if found.is_none() && control.key() == Some(key) {
                    found = Some(control);
                }
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

fn assign_control_keys(nodes: &mut [MessageNode]) {
    let mut next = 1_u32;
    for node in nodes {
        visit_node_controls_mut(node, &mut |control| {
            control.key = Some(MessageControlKey(next));
            next = next
                .checked_add(1)
                .expect("message control key space exhausted");
        });
    }
}

fn visit_node_controls_mut(node: &mut MessageNode, visitor: &mut impl FnMut(&mut MessageControl)) {
    match node {
        MessageNode::Control(control) => visitor(control),
        MessageNode::Section {
            accessory: Some(MessageAccessory::Control(control)),
            ..
        } => visitor(control),
        MessageNode::Actions(controls) => controls.iter_mut().for_each(visitor),
        MessageNode::Attachment(attachment) => attachment.actions.iter_mut().for_each(visitor),
        _ => {}
    }
}

fn visit_node_controls<'a>(node: &'a MessageNode, visitor: &mut impl FnMut(&'a MessageControl)) {
    match node {
        MessageNode::Control(control) => visitor(control),
        MessageNode::Section {
            accessory: Some(MessageAccessory::Control(control)),
            ..
        } => visitor(control),
        MessageNode::Actions(controls) => controls.iter().for_each(visitor),
        MessageNode::Attachment(attachment) => attachment.actions.iter().for_each(visitor),
        _ => {}
    }
}

fn project_node_text(node: &MessageNode, include_controls: bool, parts: &mut Vec<String>) {
    match node {
        MessageNode::Text(text) | MessageNode::Header(text) => push_text(parts, text),
        MessageNode::Control(control) if include_controls => push_text(parts, control.label()),
        MessageNode::Section { text, fields, .. } => {
            if let Some(text) = text {
                push_text(parts, text);
            }
            for field in fields {
                push_text(parts, field);
            }
        }
        MessageNode::Context(elements) => {
            for element in elements {
                push_text(parts, element);
            }
        }
        MessageNode::Image(image) => push_text(parts, &image.alt),
        MessageNode::Actions(controls) if include_controls => {
            for control in controls {
                push_text(parts, control.label());
            }
        }
        MessageNode::RichText(nodes) => {
            for node in nodes {
                for inline in rich_text_inlines(node) {
                    match inline {
                        RichInline::Text { text, .. } => push_text(parts, text),
                        RichInline::Link { label, .. } => push_text(parts, label),
                        RichInline::User(id) => push_text(parts, id),
                        RichInline::Channel(id) => push_text(parts, id),
                        RichInline::Emoji(name) => push_text(parts, name),
                    }
                }
            }
        }
        MessageNode::Attachment(attachment) => {
            let start = parts.len();
            for value in [
                attachment.pretext.as_deref(),
                attachment.author.as_ref().map(|value| value.text.as_str()),
                attachment.title.as_ref().map(|value| value.text.as_str()),
                attachment.text.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                push_text(parts, value);
            }
            for field in &attachment.fields {
                if let Some(title) = field.title.as_deref() {
                    push_text(parts, title);
                }
                if let Some(value) = field.value.as_deref() {
                    push_text(parts, value);
                }
            }
            if include_controls {
                for control in &attachment.actions {
                    push_text(parts, control.label());
                }
            }
            if parts.len() == start {
                if let Some(fallback) = attachment.fallback.as_deref() {
                    push_text(parts, fallback);
                }
            }
        }
        MessageNode::Unsupported {
            fallback: Some(text),
            ..
        } => push_text(parts, text),
        MessageNode::Control(_)
        | MessageNode::Actions(_)
        | MessageNode::Divider
        | MessageNode::Unsupported { fallback: None, .. } => {}
    }
}

fn push_text(parts: &mut Vec<String>, value: &str) {
    if let Some(value) = non_empty(value) {
        parts.push(value.to_string());
    }
}

fn rich_text_inlines(node: &RichTextNode) -> Box<dyn Iterator<Item = &RichInline> + '_> {
    match node {
        RichTextNode::Paragraph(inlines)
        | RichTextNode::Preformatted(inlines)
        | RichTextNode::Quote(inlines) => Box::new(inlines.iter()),
        RichTextNode::List { items, .. } => Box::new(items.iter().flatten()),
    }
}

fn message_node_user_ids(node: &MessageNode) -> Box<dyn Iterator<Item = &str> + '_> {
    match node {
        MessageNode::RichText(nodes) => Box::new(
            nodes
                .iter()
                .flat_map(rich_text_inlines)
                .filter_map(|inline| match inline {
                    RichInline::User(id) => Some(id.as_str()),
                    _ => None,
                }),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_values_are_redacted_from_debug_output() {
        let value = SensitiveValue::new("synthetic-private-marker");
        let document = MessageDocument::new(
            vec![MessageNode::Control(MessageControl::new(
                "Approve",
                Some(value.clone()),
            ))],
            None,
        );

        assert!(!format!("{value:?}").contains("synthetic-private-marker"));
        assert!(!format!("{document:?}").contains("synthetic-private-marker"));
    }

    #[test]
    fn document_assigns_stable_keys_across_control_locations() {
        let document = MessageDocument::new(
            vec![
                MessageNode::Control(MessageControl::new("First", None)),
                MessageNode::Section {
                    text: None,
                    fields: Vec::new(),
                    accessory: Some(MessageAccessory::Control(MessageControl::new(
                        "Second", None,
                    ))),
                },
                MessageNode::Actions(vec![MessageControl::new("Third", None)]),
            ],
            None,
        );
        let keys = document.control_keys();

        assert_eq!(keys.len(), 3);
        assert_eq!(
            keys.iter().map(|key| key.0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            document.control(keys[1]).map(MessageControl::label),
            Some("Second")
        );
    }

    #[test]
    fn callback_action_is_redacted_and_not_serialized() {
        let document = MessageDocument::new(
            vec![MessageNode::Control(MessageControl::callback(
                "Approve",
                SlackControlAction::Block {
                    action: SensitiveValue::new("synthetic-private-action"),
                },
                None,
            ))],
            None,
        );

        assert!(!format!("{document:?}").contains("synthetic-private-action"));
        assert!(!serde_json::to_string(&document)
            .expect("document serializes")
            .contains("synthetic-private-action"));
    }

    #[test]
    fn document_projections_agree_about_attachment_only_content() {
        let document = MessageDocument::new(
            vec![
                MessageNode::Text("Request review".to_string()),
                MessageNode::Control(MessageControl::new("Approve", None)),
            ],
            None,
        );

        assert!(document.has_visible_content());
        assert_eq!(document.visible_text(), "Request review");
        assert_eq!(document.accessible_text(), "Request review\nApprove");
    }

    #[test]
    fn image_urls_cover_blocks_accessories_and_attachments() {
        let image = |url: &str| MessageImage {
            url: Some(url.to_string()),
            alt: "Preview".to_string(),
            title: None,
        };
        let document = MessageDocument::new(
            vec![
                MessageNode::Image(image("https://files.slack.com/block.png")),
                MessageNode::Section {
                    text: None,
                    fields: Vec::new(),
                    accessory: Some(MessageAccessory::Image(image(
                        "https://files.slack.com/accessory.png",
                    ))),
                },
                MessageNode::Attachment(Box::new(MessageAttachment {
                    color: None,
                    pretext: None,
                    author: None,
                    title: None,
                    text: None,
                    fallback: None,
                    fields: Vec::new(),
                    image: Some(image("https://files.slack.com/attachment.png")),
                    actions: Vec::new(),
                    footer: None,
                })),
            ],
            None,
        );

        assert_eq!(
            document.image_urls().collect::<Vec<_>>(),
            vec![
                "https://files.slack.com/block.png",
                "https://files.slack.com/accessory.png",
                "https://files.slack.com/attachment.png",
            ]
        );
    }
}
