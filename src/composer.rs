/* composer.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::models::{SlackMessage, SlackUser};
use crate::rich_message::{MessageNode, RichInline, RichInlineStyle, RichTextNode};
use crate::search::{
    SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT,
};

pub fn text_view_text(text_view: &gtk::TextView) -> String {
    let buffer = text_view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub user_id: String,
    pub display_name: String,
    pub full_name: Option<String>,
    pub username: Option<String>,
    pub search_aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionSpan {
    pub start: usize,
    pub end: usize,
    pub user_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerEntityKind {
    Link { url: String },
    Channel { channel_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerEntitySpan {
    pub start: usize,
    pub end: usize,
    pub label: String,
    pub kind: ComposerEntityKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionInsertion {
    pub text: String,
    pub caret: usize,
    pub span: MentionSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedComposerText {
    pub text: String,
    pub mentions: Vec<MentionSpan>,
}

const RICH_COMPOSER_DRAFT_PREFIX: &str = "conduit-rich-v1:";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComposerTextStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
}

impl ComposerTextStyle {
    fn merge(&mut self, other: Self) {
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.underline |= other.underline;
        self.strike |= other.strike;
        self.code |= other.code;
    }

    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerStyleSpan {
    pub start: usize,
    pub end: usize,
    pub style: ComposerTextStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerBlockKind {
    BulletedList,
    NumberedList,
    Quote,
    Preformatted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerBlockSpan {
    pub start: usize,
    pub end: usize,
    pub kind: ComposerBlockKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerAttachmentDraft {
    pub path: PathBuf,
    pub remove_after_upload: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RichComposerDraft {
    pub text: String,
    pub mentions: Vec<MentionSpan>,
    pub entities: Vec<ComposerEntitySpan>,
    pub styles: Vec<ComposerStyleSpan>,
    pub blocks: Vec<ComposerBlockSpan>,
    pub attachments: Vec<ComposerAttachmentDraft>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerMessagePayload {
    pub fallback_text: String,
    pub blocks_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComposerLine {
    start: usize,
    end: usize,
}

impl RichComposerDraft {
    pub fn slack_payload(&self) -> Option<ComposerMessagePayload> {
        if self.text.trim().is_empty() {
            return None;
        }

        let elements = composer_rich_text_elements(self);
        let blocks = vec![json!({
            "type": "rich_text",
            "elements": elements,
        })];
        Some(ComposerMessagePayload {
            fallback_text: serialize_composer_semantics(&self.text, &self.mentions, &self.entities),
            blocks_json: serde_json::to_string(&blocks).ok()?,
        })
    }
}

#[derive(Default)]
struct MessageDraftBuilder {
    draft: RichComposerDraft,
    position: usize,
    has_line: bool,
}

impl MessageDraftBuilder {
    fn start_line(&mut self) -> usize {
        if self.has_line {
            self.draft.text.push('\n');
            self.position += 1;
        }
        self.has_line = true;
        self.position
    }

    fn push_text(&mut self, text: &str, style: RichInlineStyle) {
        let start = self.position;
        self.draft.text.push_str(text);
        self.position += text.chars().count();
        let style = ComposerTextStyle {
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            strike: style.strike,
            code: style.code,
        };
        if start < self.position && !style.is_empty() {
            self.draft.styles.push(ComposerStyleSpan {
                start,
                end: self.position,
                style,
            });
        }
    }

    fn push_inlines(&mut self, inlines: &[RichInline], user_names: &HashMap<String, String>) {
        for inline in inlines {
            match inline {
                RichInline::Text { text, style } => self.push_text(text, *style),
                RichInline::Link { url, label, style } => {
                    let label = if label.is_empty() { url } else { label };
                    let start = self.position;
                    self.push_text(label, *style);
                    self.draft.entities.push(ComposerEntitySpan {
                        start,
                        end: self.position,
                        label: label.clone(),
                        kind: ComposerEntityKind::Link { url: url.clone() },
                    });
                }
                RichInline::User(user_id) => {
                    let label = format!(
                        "@{}",
                        user_names
                            .get(user_id)
                            .map(String::as_str)
                            .unwrap_or(user_id)
                    );
                    let start = self.position;
                    self.draft.text.push_str(&label);
                    self.position += label.chars().count();
                    self.draft.mentions.push(MentionSpan {
                        start,
                        end: self.position,
                        user_id: user_id.clone(),
                        label,
                    });
                }
                RichInline::Channel(channel_id) => {
                    let label = format!("#{channel_id}");
                    let start = self.position;
                    self.push_text(&label, RichInlineStyle::default());
                    self.draft.entities.push(ComposerEntitySpan {
                        start,
                        end: self.position,
                        label,
                        kind: ComposerEntityKind::Channel {
                            channel_id: channel_id.clone(),
                        },
                    });
                }
                RichInline::Emoji(name) => {
                    self.push_text(&emoji_shortcode(name), RichInlineStyle::default())
                }
            }
        }
    }

    fn push_line(
        &mut self,
        inlines: &[RichInline],
        kind: Option<ComposerBlockKind>,
        user_names: &HashMap<String, String>,
    ) {
        let start = self.start_line();
        self.push_inlines(inlines, user_names);
        if let Some(kind) = kind.filter(|_| start < self.position) {
            self.draft.blocks.push(ComposerBlockSpan {
                start,
                end: self.position,
                kind,
            });
        }
    }
}

fn blocks_match_editable_draft(message: &SlackMessage, draft: &RichComposerDraft) -> bool {
    let Some(Value::Array(original_blocks)) = message.blocks.as_ref() else {
        return false;
    };
    let Some(payload) = draft.slack_payload() else {
        return false;
    };
    let Ok(Value::Array(generated_blocks)) = serde_json::from_str::<Value>(&payload.blocks_json)
    else {
        return false;
    };
    let normalized_original = original_blocks
        .iter()
        .cloned()
        .map(|mut block| {
            if let Some(block) = block.as_object_mut() {
                block.remove("block_id");
            }
            block
        })
        .collect::<Vec<_>>();
    normalized_original == generated_blocks
}

/// Converts a plain or wholly rich-text Slack message into an editable composer draft.
/// Mixed Block Kit layouts are rejected so editing cannot silently discard content.
pub fn composer_draft_from_message(
    message: &SlackMessage,
    user_names: &HashMap<String, String>,
) -> Option<(RichComposerDraft, bool)> {
    let blocks_are_editable = match message.blocks.as_ref() {
        None => true,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .all(|block| block.get("type").and_then(Value::as_str) == Some("rich_text")),
        Some(_) => false,
    };
    if !blocks_are_editable {
        return None;
    }
    let document_is_editable = message
        .document
        .nodes()
        .iter()
        .all(|node| matches!(node, MessageNode::RichText(_)));
    if !document_is_editable {
        return None;
    }
    let uses_rich_blocks = message
        .document
        .nodes()
        .iter()
        .any(|node| matches!(node, MessageNode::RichText(_)));

    let mut builder = MessageDraftBuilder::default();
    for node in message.document.nodes() {
        let MessageNode::RichText(nodes) = node else {
            continue;
        };
        for node in nodes {
            match node {
                RichTextNode::Paragraph(inlines) => builder.push_line(inlines, None, user_names),
                RichTextNode::Preformatted(inlines) => {
                    builder.push_line(inlines, Some(ComposerBlockKind::Preformatted), user_names)
                }
                RichTextNode::Quote(inlines) => {
                    builder.push_line(inlines, Some(ComposerBlockKind::Quote), user_names)
                }
                RichTextNode::List { ordered, items } => {
                    let kind = if *ordered {
                        ComposerBlockKind::NumberedList
                    } else {
                        ComposerBlockKind::BulletedList
                    };
                    for item in items {
                        builder.push_line(item, Some(kind), user_names);
                    }
                }
            }
        }
    }
    if !builder.draft.text.is_empty() {
        if uses_rich_blocks && !blocks_match_editable_draft(message, &builder.draft) {
            return None;
        }
        return Some((builder.draft, uses_rich_blocks));
    }
    if uses_rich_blocks {
        return None;
    }

    let hydrated = hydrate_composer_mentions(&message.body_text(), user_names);
    Some((
        RichComposerDraft {
            text: hydrated.text,
            mentions: hydrated.mentions,
            ..RichComposerDraft::default()
        },
        uses_rich_blocks,
    ))
}

pub fn encode_rich_composer_draft(draft: &RichComposerDraft) -> String {
    serde_json::to_string(draft)
        .map(|draft| format!("{RICH_COMPOSER_DRAFT_PREFIX}{draft}"))
        .unwrap_or_default()
}

pub fn decode_rich_composer_draft(stored: &str) -> Option<RichComposerDraft> {
    serde_json::from_str(stored.strip_prefix(RICH_COMPOSER_DRAFT_PREFIX)?).ok()
}

fn composer_lines(text: &str) -> Vec<ComposerLine> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, character) in characters.iter().enumerate() {
        if *character == '\n' {
            lines.push(ComposerLine { start, end: index });
            start = index + 1;
        }
    }
    lines.push(ComposerLine {
        start,
        end: characters.len(),
    });
    lines
}

fn composer_line_kind(
    blocks: &[ComposerBlockSpan],
    line: ComposerLine,
    text_length: usize,
) -> Option<ComposerBlockKind> {
    blocks
        .iter()
        .filter(|span| span.start < span.end && span.end <= text_length)
        .find(|span| span.start <= line.start && span.end >= line.end)
        .map(|span| span.kind)
}

fn composer_rich_text_elements(draft: &RichComposerDraft) -> Vec<Value> {
    let text_length = draft.text.chars().count();
    let lines = composer_lines(&draft.text);
    let mut elements = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let kind = composer_line_kind(&draft.blocks, lines[index], text_length);
        let mut end_index = index + 1;
        while end_index < lines.len()
            && composer_line_kind(&draft.blocks, lines[end_index], text_length) == kind
        {
            end_index += 1;
        }

        match kind {
            Some(ComposerBlockKind::BulletedList | ComposerBlockKind::NumberedList) => {
                let style = if kind == Some(ComposerBlockKind::BulletedList) {
                    "bullet"
                } else {
                    "ordered"
                };
                let items = lines[index..end_index]
                    .iter()
                    .map(|line| {
                        json!({
                            "type": "rich_text_section",
                            "elements": composer_inline_elements(draft, line.start, line.end),
                        })
                    })
                    .collect::<Vec<_>>();
                elements.push(json!({
                    "type": "rich_text_list",
                    "style": style,
                    "indent": 0,
                    "elements": items,
                }));
            }
            Some(ComposerBlockKind::Quote | ComposerBlockKind::Preformatted) => {
                let element_type = if kind == Some(ComposerBlockKind::Quote) {
                    "rich_text_quote"
                } else {
                    "rich_text_preformatted"
                };
                elements.push(json!({
                    "type": element_type,
                    "elements": composer_inline_elements(
                        draft,
                        lines[index].start,
                        lines[end_index - 1].end,
                    ),
                }));
            }
            None => {
                elements.push(json!({
                    "type": "rich_text_section",
                    "elements": composer_inline_elements(
                        draft,
                        lines[index].start,
                        lines[end_index - 1].end,
                    ),
                }));
            }
        }
        index = end_index;
    }

    elements
}

fn composer_inline_elements(draft: &RichComposerDraft, start: usize, end: usize) -> Vec<Value> {
    let characters = draft.text.chars().collect::<Vec<_>>();
    let start = start.min(characters.len());
    let end = end.min(characters.len()).max(start);
    let mentions = valid_composer_mentions(&draft.text, &draft.mentions);
    let entities = valid_composer_entities(&draft.text, &draft.entities)
        .into_iter()
        .filter(|entity| {
            !mentions
                .iter()
                .any(|mention| ranges_overlap(entity.start, entity.end, mention.start, mention.end))
        })
        .collect::<Vec<_>>();
    let styles = draft
        .styles
        .iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= characters.len()
                && !span.style.is_empty()
                && span.start < end
                && start < span.end
        })
        .collect::<Vec<_>>();
    let mut boundaries = vec![start, end];
    for span in &styles {
        for boundary in [span.start.max(start), span.end.min(end)] {
            let splits_semantic = mentions
                .iter()
                .any(|mention| mention.start < boundary && boundary < mention.end)
                || entities
                    .iter()
                    .any(|entity| entity.start < boundary && boundary < entity.end);
            if !splits_semantic {
                boundaries.push(boundary);
            }
        }
    }
    for mention in &mentions {
        if mention.start < end && start < mention.end {
            boundaries.push(mention.start.max(start));
            boundaries.push(mention.end.min(end));
        }
    }
    for entity in &entities {
        if entity.start < end && start < entity.end {
            boundaries.push(entity.start.max(start));
            boundaries.push(entity.end.min(end));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut elements = Vec::new();
    for pair in boundaries.windows(2) {
        let segment_start = pair[0];
        let segment_end = pair[1];
        if segment_start == segment_end {
            continue;
        }
        let mut style = ComposerTextStyle::default();
        for span in &styles {
            if span.start <= segment_start && span.end >= segment_end {
                style.merge(span.style);
            }
        }
        if let Some(mention) = mentions
            .iter()
            .find(|mention| mention.start == segment_start && mention.end == segment_end)
        {
            elements.push(styled_rich_element(
                json!({"type": "user", "user_id": mention.user_id}),
                style,
            ));
            continue;
        }
        if let Some(entity) = entities
            .iter()
            .find(|entity| entity.start == segment_start && entity.end == segment_end)
        {
            let element = match &entity.kind {
                ComposerEntityKind::Link { url } => {
                    json!({"type": "link", "url": url, "text": entity.label})
                }
                ComposerEntityKind::Channel { channel_id } => {
                    json!({"type": "channel", "channel_id": channel_id})
                }
            };
            elements.push(styled_rich_element(element, style));
            continue;
        }

        let text = characters[segment_start..segment_end]
            .iter()
            .collect::<String>();
        elements.extend(composer_text_elements(&text, style));
    }
    elements
}

fn composer_text_elements(text: &str, style: ComposerTextStyle) -> Vec<Value> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut elements = Vec::new();
    let mut cursor = 0;
    let mut plain_start = 0;
    while cursor < characters.len() {
        if characters[cursor] != ':' {
            cursor += 1;
            continue;
        }
        let Some(relative_end) = characters[cursor + 1..]
            .iter()
            .position(|character| *character == ':')
        else {
            cursor += 1;
            continue;
        };
        let shortcode_end = cursor + 1 + relative_end;
        let name = characters[cursor + 1..shortcode_end]
            .iter()
            .collect::<String>();
        let valid = !name.is_empty() && name.chars().all(is_shortcode_character);
        if !valid {
            cursor += 1;
            continue;
        }
        if plain_start < cursor {
            elements.push(styled_rich_element(
                json!({
                    "type": "text",
                    "text": characters[plain_start..cursor].iter().collect::<String>(),
                }),
                style,
            ));
        }
        elements.push(styled_rich_element(
            json!({"type": "emoji", "name": name}),
            style,
        ));
        cursor = shortcode_end + 1;
        plain_start = cursor;
    }
    if plain_start < characters.len() {
        elements.push(styled_rich_element(
            json!({
                "type": "text",
                "text": characters[plain_start..].iter().collect::<String>(),
            }),
            style,
        ));
    }
    elements
}

fn styled_rich_element(mut element: Value, style: ComposerTextStyle) -> Value {
    if style.is_empty() {
        return element;
    }
    let mut style_json = Map::new();
    for (name, enabled) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underline", style.underline),
        ("strike", style.strike),
        ("code", style.code),
    ] {
        if enabled {
            style_json.insert(name.to_string(), Value::Bool(true));
        }
    }
    if let Some(object) = element.as_object_mut() {
        object.insert("style".to_string(), Value::Object(style_json));
    }
    element
}

fn is_shortcode_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
}

fn is_shortcode_boundary(character: char) -> bool {
    !is_shortcode_character(character) && !matches!(character, ':' | '/' | '\\')
}

pub fn emoji_token_at_caret(text: &str, caret: usize) -> Option<EmojiToken> {
    let characters = text.chars().collect::<Vec<_>>();
    if caret > characters.len()
        || caret < 3
        || characters
            .get(caret)
            .is_some_and(|character| is_shortcode_character(*character) || *character == ':')
    {
        return None;
    }

    let mut query_start = caret;
    while query_start > 0 && is_shortcode_character(characters[query_start - 1]) {
        query_start -= 1;
    }
    let colon = query_start.checked_sub(1)?;
    if characters[colon] != ':' || (colon > 0 && !is_shortcode_boundary(characters[colon - 1])) {
        return None;
    }

    let query = characters[query_start..caret].iter().collect::<String>();
    if query.chars().count() < 2 {
        return None;
    }

    Some(EmojiToken {
        start: colon,
        end: caret,
        query,
    })
}

fn is_mention_query_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '\'')
}

fn is_mention_boundary(character: char) -> bool {
    !character.is_alphanumeric() && !matches!(character, '_' | '@' | '<' | '/' | '\\')
}

pub fn mention_token_at_caret(text: &str, caret: usize) -> Option<MentionToken> {
    let characters = text.chars().collect::<Vec<_>>();
    if caret > characters.len()
        || characters
            .get(caret)
            .is_some_and(|character| is_mention_query_character(*character) || *character == '@')
    {
        return None;
    }

    let mut query_start = caret;
    while query_start > 0 && is_mention_query_character(characters[query_start - 1]) {
        query_start -= 1;
    }
    let at = query_start.checked_sub(1)?;
    if characters[at] != '@' || (at > 0 && !is_mention_boundary(characters[at - 1])) {
        return None;
    }

    Some(MentionToken {
        start: at,
        end: caret,
        query: characters[query_start..caret].iter().collect(),
    })
}

fn valid_user_id(user_id: &str) -> bool {
    !user_id.is_empty()
        && user_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn trimmed_owned(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn add_unique_alias(aliases: &mut Vec<String>, alias: &str) {
    let alias = alias.trim();
    if !alias.is_empty()
        && !aliases
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(alias))
    {
        aliases.push(alias.to_string());
    }
}

pub fn mention_candidates(users: &[SlackUser]) -> Vec<MentionCandidate> {
    let mut candidates_by_id: HashMap<String, MentionCandidate> = HashMap::new();

    for user in users {
        if user.deleted.unwrap_or(false) || user.is_bot.unwrap_or(false) {
            continue;
        }
        let Some(user_id) = user.id.as_deref().filter(|user_id| valid_user_id(user_id)) else {
            continue;
        };
        let Some(display_name) = trimmed_owned(user.display_name()) else {
            continue;
        };
        let full_name = trimmed_owned(user.full_name());
        let username = trimmed_owned(user.name.clone());
        let aliases = user
            .search_aliases()
            .into_iter()
            .filter_map(|alias| trimmed_owned(Some(alias)))
            .collect::<Vec<_>>();

        match candidates_by_id.entry(user_id.to_string()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(MentionCandidate {
                    user_id: user_id.to_string(),
                    display_name,
                    full_name,
                    username,
                    search_aliases: aliases,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let candidate = entry.get_mut();
                if candidate.full_name.is_none() {
                    candidate.full_name = full_name;
                }
                if candidate.username.is_none() {
                    candidate.username = username;
                }
                for alias in aliases {
                    add_unique_alias(&mut candidate.search_aliases, &alias);
                }
            }
        }
    }

    let mut candidates = candidates_by_id.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.user_id.cmp(&right.user_id))
    });
    candidates
}

pub fn search_mention_candidates(
    candidates: &[MentionCandidate],
    query: &str,
    limit: usize,
) -> Vec<MentionCandidate> {
    if limit == 0 {
        return Vec::new();
    }

    let query = SearchQuery::parse(query);
    let mut matches = candidates
        .iter()
        .filter_map(|candidate| {
            let score = query.score(
                [
                    SearchField::new(&candidate.display_name, PRIMARY_FIELD_WEIGHT),
                    SearchField::new(&candidate.user_id, ID_FIELD_WEIGHT),
                ]
                .into_iter()
                .chain(
                    candidate
                        .full_name
                        .iter()
                        .map(|name| SearchField::new(name, SECONDARY_FIELD_WEIGHT)),
                )
                .chain(
                    candidate
                        .username
                        .iter()
                        .map(|name| SearchField::new(name, SECONDARY_FIELD_WEIGHT)),
                )
                .chain(
                    candidate
                        .search_aliases
                        .iter()
                        .map(|alias| SearchField::new(alias, SECONDARY_FIELD_WEIGHT)),
                ),
            )?;
            Some((candidate.clone(), score))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| {
                left.display_name
                    .to_lowercase()
                    .cmp(&right.display_name.to_lowercase())
            })
            .then_with(|| left.user_id.cmp(&right.user_id))
    });

    let mut seen_user_ids = HashSet::new();
    matches
        .into_iter()
        .filter_map(|(candidate, _)| {
            seen_user_ids
                .insert(candidate.user_id.clone())
                .then_some(candidate)
        })
        .take(limit)
        .collect()
}

pub fn replace_mention_token(
    text: &str,
    token: &MentionToken,
    candidate: &MentionCandidate,
) -> MentionInsertion {
    let mut characters = text.chars().collect::<Vec<_>>();
    let end = token.end.min(characters.len());
    let start = token.start.min(end);
    let display_name = match candidate.display_name.trim() {
        "" => candidate.user_id.as_str(),
        display_name => display_name,
    };
    let label = format!("@{display_name}");
    let label_characters = label.chars().collect::<Vec<_>>();
    let append_space = end == characters.len();
    let mut replacement = label_characters.clone();
    if append_space {
        replacement.push(' ');
    }
    characters.splice(start..end, replacement.iter().copied());

    let span_end = start + label_characters.len();
    MentionInsertion {
        text: characters.into_iter().collect(),
        caret: span_end + usize::from(append_space),
        span: MentionSpan {
            start,
            end: span_end,
            user_id: candidate.user_id.clone(),
            label,
        },
    }
}

fn valid_mention_span(span: &MentionSpan, characters: &[char]) -> bool {
    valid_user_id(&span.user_id)
        && span.start < span.end
        && span.end <= characters.len()
        && span.label.starts_with('@')
        && characters[span.start..span.end].iter().collect::<String>() == span.label
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start < right_end && right_start < left_end
}

pub fn expand_composer_semantic_selection(
    mut start: usize,
    mut end: usize,
    mentions: &[MentionSpan],
    entities: &[ComposerEntitySpan],
) -> (usize, usize) {
    loop {
        let previous = (start, end);
        for (span_start, span_end) in mentions
            .iter()
            .map(|span| (span.start, span.end))
            .chain(entities.iter().map(|span| (span.start, span.end)))
        {
            if ranges_overlap(start, end, span_start, span_end) {
                start = start.min(span_start);
                end = end.max(span_end);
            }
        }
        if (start, end) == previous {
            return (start, end);
        }
    }
}

fn valid_composer_mentions<'a>(text: &str, spans: &'a [MentionSpan]) -> Vec<&'a MentionSpan> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut valid = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| valid_mention_span(span, &characters))
        .filter(|(index, span)| {
            !spans.iter().enumerate().any(|(other_index, other)| {
                *index != other_index
                    && valid_mention_span(other, &characters)
                    && ranges_overlap(span.start, span.end, other.start, other.end)
            })
        })
        .map(|(_, span)| span)
        .collect::<Vec<_>>();
    valid.sort_by_key(|span| (span.start, span.end));
    valid
}

fn valid_entity_span(span: &ComposerEntitySpan, characters: &[char]) -> bool {
    let identity_is_valid = match &span.kind {
        ComposerEntityKind::Link { url } => !url.trim().is_empty(),
        ComposerEntityKind::Channel { channel_id } => !channel_id.trim().is_empty(),
    };
    identity_is_valid
        && span.start < span.end
        && span.end <= characters.len()
        && characters[span.start..span.end].iter().collect::<String>() == span.label
}

fn valid_composer_entities<'a>(
    text: &str,
    spans: &'a [ComposerEntitySpan],
) -> Vec<&'a ComposerEntitySpan> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut valid = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| valid_entity_span(span, &characters))
        .filter(|(index, span)| {
            !spans.iter().enumerate().any(|(other_index, other)| {
                *index != other_index
                    && valid_entity_span(other, &characters)
                    && ranges_overlap(span.start, span.end, other.start, other.end)
            })
        })
        .map(|(_, span)| span)
        .collect::<Vec<_>>();
    valid.sort_by_key(|span| (span.start, span.end));
    valid
}

pub fn serialize_composer_mentions(text: &str, spans: &[MentionSpan]) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    let valid = valid_composer_mentions(text, spans);

    let mut serialized = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in valid {
        serialized.extend(characters[cursor..span.start].iter());
        serialized.push_str("<@");
        serialized.push_str(&span.user_id);
        serialized.push('>');
        cursor = span.end;
    }
    serialized.extend(characters[cursor..].iter());
    serialized
}

pub fn serialize_composer_semantics(
    text: &str,
    mentions: &[MentionSpan],
    entities: &[ComposerEntitySpan],
) -> String {
    if entities.is_empty() {
        return serialize_composer_mentions(text, mentions);
    }

    enum SemanticSpan<'a> {
        Mention(&'a MentionSpan),
        Entity(&'a ComposerEntitySpan),
    }

    impl SemanticSpan<'_> {
        fn start(&self) -> usize {
            match self {
                Self::Mention(span) => span.start,
                Self::Entity(span) => span.start,
            }
        }

        fn end(&self) -> usize {
            match self {
                Self::Mention(span) => span.end,
                Self::Entity(span) => span.end,
            }
        }
    }

    let characters = text.chars().collect::<Vec<_>>();
    let mentions = valid_composer_mentions(text, mentions);
    let entities = valid_composer_entities(text, entities)
        .into_iter()
        .filter(|entity| {
            !mentions
                .iter()
                .any(|mention| ranges_overlap(entity.start, entity.end, mention.start, mention.end))
        })
        .collect::<Vec<_>>();
    let mut spans = mentions
        .into_iter()
        .map(SemanticSpan::Mention)
        .chain(entities.into_iter().map(SemanticSpan::Entity))
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start(), span.end()));

    let mut serialized = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in spans {
        serialized.extend(characters[cursor..span.start()].iter());
        match span {
            SemanticSpan::Mention(span) => {
                serialized.push_str("<@");
                serialized.push_str(&span.user_id);
                serialized.push('>');
            }
            SemanticSpan::Entity(span) => match &span.kind {
                ComposerEntityKind::Link { url } if span.label == *url => {
                    serialized.push('<');
                    serialized.push_str(url);
                    serialized.push('>');
                }
                ComposerEntityKind::Link { url } => {
                    serialized.push('<');
                    serialized.push_str(url);
                    serialized.push('|');
                    serialized.push_str(&span.label);
                    serialized.push('>');
                }
                ComposerEntityKind::Channel { channel_id } => {
                    serialized.push_str("<#");
                    serialized.push_str(channel_id);
                    serialized.push('>');
                }
            },
        }
        cursor = span.end();
    }
    serialized.extend(characters[cursor..].iter());
    serialized
}

pub fn hydrate_composer_mentions(
    text: &str,
    names: &HashMap<String, String>,
) -> HydratedComposerText {
    let characters = text.chars().collect::<Vec<_>>();
    let mut hydrated = String::with_capacity(text.len());
    let mut hydrated_length = 0;
    let mut mentions = Vec::new();
    let mut index = 0;

    while index < characters.len() {
        if characters[index] == '<' && characters.get(index + 1) == Some(&'@') {
            if let Some(relative_end) = characters[index + 2..]
                .iter()
                .position(|character| *character == '>')
            {
                let source_end = index + 2 + relative_end;
                let user_id = characters[index + 2..source_end].iter().collect::<String>();
                if valid_user_id(&user_id) {
                    let display_name = names
                        .get(&user_id)
                        .map(String::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(&user_id);
                    let label = format!("@{display_name}");
                    let start = hydrated_length;
                    hydrated.push_str(&label);
                    hydrated_length += label.chars().count();
                    mentions.push(MentionSpan {
                        start,
                        end: hydrated_length,
                        user_id,
                        label,
                    });
                    index = source_end + 1;
                    continue;
                }
            }
        }

        hydrated.push(characters[index]);
        hydrated_length += 1;
        index += 1;
    }

    HydratedComposerText {
        text: hydrated,
        mentions,
    }
}

pub fn replace_emoji_token(text: &str, token: &EmojiToken, shortcode: &str) -> (String, usize) {
    let mut characters = text.chars().collect::<Vec<_>>();
    let replacement = emoji_shortcode(shortcode).chars().collect::<Vec<_>>();
    let end = token.end.min(characters.len());
    let start = token.start.min(end);
    characters.splice(start..end, replacement.iter().copied());
    let caret = start + replacement.len();
    (characters.into_iter().collect(), caret)
}

pub fn emoji_shortcode(name: &str) -> String {
    format!(":{name}:")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKeyAction {
    Previous,
    Next,
    Accept,
    Dismiss,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEditKeyAction {
    Start,
    Cancel,
    Ignore,
}

pub fn message_edit_key_action(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
    composer_empty: bool,
    editing: bool,
) -> MessageEditKeyAction {
    let non_lock_modifiers = gtk::gdk::ModifierType::SHIFT_MASK
        | gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::ALT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
        | gtk::gdk::ModifierType::META_MASK;
    let has_only_control = state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
        && !state.intersects(
            gtk::gdk::ModifierType::SHIFT_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK
                | gtk::gdk::ModifierType::META_MASK,
        );

    if key == gtk::gdk::Key::Escape && editing && !state.intersects(non_lock_modifiers) {
        MessageEditKeyAction::Cancel
    } else if key == gtk::gdk::Key::Up && has_only_control && composer_empty && !editing {
        MessageEditKeyAction::Start
    } else {
        MessageEditKeyAction::Ignore
    }
}

pub fn completion_key_action(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
) -> CompletionKeyAction {
    match key {
        gtk::gdk::Key::Up => CompletionKeyAction::Previous,
        gtk::gdk::Key::Down => CompletionKeyAction::Next,
        gtk::gdk::Key::Escape => CompletionKeyAction::Dismiss,
        gtk::gdk::Key::Tab
            if !state.intersects(
                gtk::gdk::ModifierType::SHIFT_MASK
                    | gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SUPER_MASK,
            ) =>
        {
            CompletionKeyAction::Accept
        }
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
            if !state.intersects(
                gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK,
            ) =>
        {
            CompletionKeyAction::Accept
        }
        _ => CompletionKeyAction::Ignore,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextViewEnterAction {
    Send,
    InsertNewline,
    Ignore,
}

pub fn text_view_enter_action(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
) -> TextViewEnterAction {
    if !matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
        return TextViewEnterAction::Ignore;
    }

    if state.intersects(gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK) {
        TextViewEnterAction::InsertNewline
    } else {
        TextViewEnterAction::Send
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::models::SlackUserProfile;
    use crate::rich_message::MessageDocument;

    use super::*;

    fn user(
        id: Option<&str>,
        display_name: Option<&str>,
        full_name: Option<&str>,
        username: Option<&str>,
    ) -> SlackUser {
        SlackUser {
            id: id.map(ToString::to_string),
            name: username.map(ToString::to_string),
            real_name: full_name.map(ToString::to_string),
            profile: Some(SlackUserProfile {
                display_name: display_name.map(ToString::to_string),
                real_name: full_name.map(ToString::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn enter_action_sends_on_plain_enter() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Send
        );
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::KP_Enter, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Send
        );
    }

    #[test]
    fn enter_action_inserts_newline_with_shift_or_control() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::SHIFT_MASK),
            TextViewEnterAction::InsertNewline
        );
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::CONTROL_MASK),
            TextViewEnterAction::InsertNewline
        );
    }

    #[test]
    fn enter_action_ignores_other_keys() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::space, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Ignore
        );
    }

    #[test]
    fn edit_shortcut_requires_an_empty_composer_and_control_up() {
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Up,
                gtk::gdk::ModifierType::CONTROL_MASK,
                true,
                false,
            ),
            MessageEditKeyAction::Start
        );
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Up,
                gtk::gdk::ModifierType::CONTROL_MASK,
                false,
                false,
            ),
            MessageEditKeyAction::Ignore
        );
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Up,
                gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK,
                true,
                false,
            ),
            MessageEditKeyAction::Ignore
        );
    }

    #[test]
    fn escape_cancels_only_an_active_edit() {
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                false,
                true,
            ),
            MessageEditKeyAction::Cancel
        );
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                true,
                false,
            ),
            MessageEditKeyAction::Ignore
        );
        assert_eq!(
            message_edit_key_action(
                gtk::gdk::Key::Up,
                gtk::gdk::ModifierType::CONTROL_MASK,
                false,
                true,
            ),
            MessageEditKeyAction::Ignore
        );
    }

    #[test]
    fn formatting_selection_expands_to_complete_semantic_spans() {
        let mentions = vec![MentionSpan {
            start: 2,
            end: 6,
            user_id: "U1".into(),
            label: "@Ada".into(),
        }];
        let entities = vec![ComposerEntitySpan {
            start: 8,
            end: 12,
            label: "link".into(),
            kind: ComposerEntityKind::Link {
                url: "https://example.com".into(),
            },
        }];

        assert_eq!(
            expand_composer_semantic_selection(4, 10, &mentions, &entities),
            (2, 12)
        );
        assert_eq!(
            expand_composer_semantic_selection(13, 15, &mentions, &entities),
            (13, 15)
        );
    }

    #[test]
    fn canonical_message_rich_text_becomes_an_editable_draft() {
        let message = SlackMessage {
            text: Some("fallback".into()),
            blocks: Some(json!([{
                "type": "rich_text",
                "elements": [
                    {
                        "type": "rich_text_section",
                        "elements": [
                            {"type": "text", "text": "Hello ", "style": {"bold": true}},
                            {"type": "user", "user_id": "U1"}
                        ]
                    },
                    {
                        "type": "rich_text_quote",
                        "elements": [{"type": "text", "text": "quoted"}]
                    },
                    {
                        "type": "rich_text_list",
                        "style": "ordered",
                        "indent": 0,
                        "elements": [
                            {
                                "type": "rich_text_section",
                                "elements": [{"type": "text", "text": "first"}]
                            },
                            {
                                "type": "rich_text_section",
                                "elements": [{"type": "emoji", "name": "wave"}]
                            }
                        ]
                    }
                ]
            }])),
            document: MessageDocument::new(
                vec![MessageNode::RichText(vec![
                    RichTextNode::Paragraph(vec![
                        RichInline::Text {
                            text: "Hello ".into(),
                            style: RichInlineStyle {
                                bold: true,
                                ..RichInlineStyle::default()
                            },
                        },
                        RichInline::User("U1".into()),
                    ]),
                    RichTextNode::Quote(vec![RichInline::Text {
                        text: "quoted".into(),
                        style: RichInlineStyle::default(),
                    }]),
                    RichTextNode::List {
                        ordered: true,
                        items: vec![
                            vec![RichInline::Text {
                                text: "first".into(),
                                style: RichInlineStyle::default(),
                            }],
                            vec![RichInline::Emoji("wave".into())],
                        ],
                    },
                ])],
                Some("fallback".into()),
            ),
            ..SlackMessage::default()
        };
        let (draft, uses_rich_blocks) = composer_draft_from_message(
            &message,
            &HashMap::from([("U1".to_string(), "Ada".to_string())]),
        )
        .expect("rich-text message should be editable");
        assert!(uses_rich_blocks);

        assert_eq!(draft.text, "Hello @Ada\nquoted\nfirst\n:wave:");
        assert_eq!(
            draft.mentions,
            vec![MentionSpan {
                start: 6,
                end: 10,
                user_id: "U1".into(),
                label: "@Ada".into(),
            }]
        );
        assert_eq!(
            draft.styles,
            vec![ComposerStyleSpan {
                start: 0,
                end: 6,
                style: ComposerTextStyle {
                    bold: true,
                    ..ComposerTextStyle::default()
                },
            }]
        );
        assert_eq!(
            draft.blocks,
            vec![
                ComposerBlockSpan {
                    start: 11,
                    end: 17,
                    kind: ComposerBlockKind::Quote,
                },
                ComposerBlockSpan {
                    start: 18,
                    end: 23,
                    kind: ComposerBlockKind::NumberedList,
                },
                ComposerBlockSpan {
                    start: 24,
                    end: 30,
                    kind: ComposerBlockKind::NumberedList,
                },
            ]
        );
    }

    #[test]
    fn edit_draft_round_trips_links_and_channel_mentions() {
        let message = SlackMessage {
            text: Some("Docs in channel".into()),
            blocks: Some(json!([{
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_section",
                    "elements": [
                        {
                            "type": "link",
                            "url": "https://example.com/docs",
                            "text": "Docs"
                        },
                        {"type": "text", "text": " in "},
                        {"type": "channel", "channel_id": "C123"}
                    ]
                }]
            }])),
            document: MessageDocument::new(
                vec![MessageNode::RichText(vec![RichTextNode::Paragraph(vec![
                    RichInline::Link {
                        url: "https://example.com/docs".into(),
                        label: "Docs".into(),
                        style: RichInlineStyle::default(),
                    },
                    RichInline::Text {
                        text: " in ".into(),
                        style: RichInlineStyle::default(),
                    },
                    RichInline::Channel("C123".into()),
                ])])],
                Some("Docs in channel".into()),
            ),
            ..SlackMessage::default()
        };
        let (mut draft, uses_rich_blocks) = composer_draft_from_message(&message, &HashMap::new())
            .expect("rich-text message should be editable");
        assert!(uses_rich_blocks);
        assert_eq!(draft.text, "Docs in #C123");
        assert_eq!(draft.entities.len(), 2);
        assert_eq!(
            serialize_composer_semantics(&draft.text, &draft.mentions, &draft.entities),
            "<https://example.com/docs|Docs> in <#C123>"
        );

        draft.styles.push(ComposerStyleSpan {
            start: 1,
            end: 3,
            style: ComposerTextStyle {
                bold: true,
                ..ComposerTextStyle::default()
            },
        });
        let payload = draft
            .slack_payload()
            .expect("semantic draft should serialize");
        let blocks: Value = serde_json::from_str(&payload.blocks_json).unwrap();
        let inlines = blocks[0]["elements"][0]["elements"].as_array().unwrap();
        assert_eq!(inlines[0]["type"], "link");
        assert_eq!(inlines[0]["url"], "https://example.com/docs");
        assert!(inlines.iter().any(|inline| inline["type"] == "channel"));

        let mut cached_without_wire_blocks = message;
        cached_without_wire_blocks.blocks = None;
        assert_eq!(
            composer_draft_from_message(&cached_without_wire_blocks, &HashMap::new()),
            None
        );
    }

    #[test]
    fn edit_draft_rejects_mixed_block_layouts_but_accepts_plain_text() {
        let mixed = SlackMessage {
            text: Some("fallback".into()),
            blocks: Some(json!([
                {"type": "rich_text"},
                {"type": "section", "text": {"type": "plain_text", "text": "extra"}}
            ])),
            ..SlackMessage::default()
        };
        assert_eq!(composer_draft_from_message(&mixed, &HashMap::new()), None);

        let cached_mixed = SlackMessage {
            text: Some("fallback".into()),
            document: MessageDocument::new(
                vec![MessageNode::Section {
                    text: Some("presentation".into()),
                    fields: Vec::new(),
                    accessory: None,
                }],
                Some("fallback".into()),
            ),
            ..SlackMessage::default()
        };
        assert_eq!(
            composer_draft_from_message(&cached_mixed, &HashMap::new()),
            None
        );

        let plain = SlackMessage {
            text: Some("Hello <@U1>".into()),
            ..SlackMessage::default()
        };
        let (draft, uses_rich_blocks) = composer_draft_from_message(
            &plain,
            &HashMap::from([("U1".to_string(), "Ada".to_string())]),
        )
        .expect("plain text should be editable");
        assert!(!uses_rich_blocks);
        assert_eq!(draft.text, "Hello @Ada");
        assert_eq!(draft.mentions[0].user_id, "U1");
    }

    #[test]
    fn detects_shortcode_tokens_at_supported_boundaries() {
        assert_eq!(
            emoji_token_at_caret(":sm", 3),
            Some(EmojiToken {
                start: 0,
                end: 3,
                query: "sm".to_string(),
            })
        );
        assert_eq!(emoji_token_at_caret("hello :par", 10).unwrap().query, "par");
        assert_eq!(emoji_token_at_caret("hello (:sm", 10).unwrap().query, "sm");
        assert_eq!(emoji_token_at_caret("hello.:sm", 9).unwrap().query, "sm");
    }

    #[test]
    fn accepts_valid_shortcode_characters_and_rejects_invalid_tokens() {
        assert_eq!(emoji_token_at_caret(":s", 2), None);
        assert_eq!(emoji_token_at_caret(":12", 3).unwrap().query, "12");
        assert_eq!(emoji_token_at_caret(":a1", 3).unwrap().query, "a1");
        assert_eq!(emoji_token_at_caret(":+1", 3).unwrap().query, "+1");
        assert_eq!(emoji_token_at_caret(":sm:", 4), None);
        assert_eq!(emoji_token_at_caret("https://sm", 10), None);
        assert_eq!(emoji_token_at_caret("12:30", 5), None);
        assert_eq!(emoji_token_at_caret("word:sm", 7), None);
        assert_eq!(emoji_token_at_caret("hello :smile", 9), None);
    }

    #[test]
    fn replaces_only_the_active_token_using_character_offsets() {
        let text = "Živjo :sm there";
        let token = emoji_token_at_caret(text, 9).unwrap();
        let (updated, caret) = replace_emoji_token(text, &token, "smile");

        assert_eq!(updated, "Živjo :smile: there");
        assert_eq!(caret, 13);
    }

    #[test]
    fn completion_keys_do_not_override_modified_enter() {
        assert_eq!(
            completion_key_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty()),
            CompletionKeyAction::Accept
        );
        assert_eq!(
            completion_key_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::SHIFT_MASK),
            CompletionKeyAction::Ignore
        );
        assert_eq!(
            completion_key_action(gtk::gdk::Key::Down, gtk::gdk::ModifierType::empty()),
            CompletionKeyAction::Next
        );
        assert_eq!(
            completion_key_action(gtk::gdk::Key::Escape, gtk::gdk::ModifierType::empty()),
            CompletionKeyAction::Dismiss
        );
    }

    #[test]
    fn detects_mentions_at_boundaries_with_supported_unicode_query_characters() {
        assert_eq!(
            mention_token_at_caret("@", 1),
            Some(MentionToken {
                start: 0,
                end: 1,
                query: String::new(),
            })
        );
        assert_eq!(
            mention_token_at_caret("hello @Žilvinas.O'Neil-2_", 25),
            Some(MentionToken {
                start: 6,
                end: 25,
                query: "Žilvinas.O'Neil-2_".to_string(),
            })
        );
        assert_eq!(
            mention_token_at_caret("hello (@ada", 11).unwrap().query,
            "ada"
        );
    }

    #[test]
    fn rejects_email_markup_invalid_query_and_caret_middle_mentions() {
        assert_eq!(mention_token_at_caret("word@ada", 8), None);
        assert_eq!(mention_token_at_caret("ada@example.com", 15), None);
        assert_eq!(mention_token_at_caret("<@U123", 6), None);
        assert_eq!(mention_token_at_caret("https://@ada", 12), None);
        assert_eq!(mention_token_at_caret("\\@ada", 5), None);
        assert_eq!(mention_token_at_caret("@ada/name", 9), None);
        assert_eq!(mention_token_at_caret("@ada", 2), None);
        assert_eq!(mention_token_at_caret("@ada", 99), None);
    }

    #[test]
    fn mention_candidates_filter_malformed_people_and_deduplicate_ids() {
        let mut duplicate = user(Some("U1"), Some("Ada"), Some("Ada Lovelace"), Some("ada"));
        duplicate.profile.as_mut().unwrap().display_name_normalized =
            Some("Ada Normalized".to_string());
        let users = vec![
            duplicate,
            user(
                Some("U1"),
                Some("Ada Duplicate"),
                None,
                Some("ada.duplicate"),
            ),
            user(Some("W2"), None, Some("Grace Hopper"), Some("grace")),
            SlackUser {
                deleted: Some(true),
                ..user(Some("U3"), Some("Deleted"), None, None)
            },
            SlackUser {
                is_bot: Some(true),
                ..user(Some("U4"), Some("Bot"), None, None)
            },
            user(None, Some("Missing ID"), None, None),
            user(Some("U-5"), Some("Invalid ID"), None, None),
            user(Some("U6"), None, None, None),
        ];

        let candidates = mention_candidates(&users);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["U1", "W2"]
        );
        assert_eq!(candidates[0].display_name, "Ada");
        assert_eq!(candidates[0].full_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(candidates[0].username.as_deref(), Some("ada"));
        assert!(candidates[0]
            .search_aliases
            .iter()
            .any(|alias| alias == "Ada Normalized"));
    }

    #[test]
    fn mention_search_ranks_all_candidates_before_limiting() {
        let candidates = vec![
            MentionCandidate {
                user_id: "U3".to_string(),
                display_name: "Zada".to_string(),
                full_name: None,
                username: None,
                search_aliases: Vec::new(),
            },
            MentionCandidate {
                user_id: "U2".to_string(),
                display_name: "Ada".to_string(),
                full_name: Some("Ada Lovelace".to_string()),
                username: Some("ada.dev".to_string()),
                search_aliases: vec!["Áda".to_string()],
            },
            MentionCandidate {
                user_id: "U1".to_string(),
                display_name: "Ada".to_string(),
                full_name: Some("Ada Byron".to_string()),
                username: Some("byron".to_string()),
                search_aliases: Vec::new(),
            },
        ];

        let limited = search_mention_candidates(&candidates, "ada", 1);
        assert_eq!(limited[0].user_id, "U1");
        assert_eq!(
            search_mention_candidates(&candidates, "lov", 10)[0].user_id,
            "U2"
        );
        assert_eq!(
            search_mention_candidates(&candidates, "ada.dev", 10)[0].user_id,
            "U2"
        );
        assert!(search_mention_candidates(&candidates, "ada", 0).is_empty());
    }

    #[test]
    fn mention_search_matches_diacritics_and_orders_duplicate_names_by_id() {
        let mut zilvinas = user(
            Some("U3"),
            Some("Žilvinas"),
            Some("Žilvinas Kuusas"),
            Some("zilvinas"),
        );
        zilvinas.profile.as_mut().unwrap().display_name_normalized = Some("Zilvinas".to_string());
        let candidates = mention_candidates(&[
            user(Some("U2"), Some("Ada"), None, None),
            user(Some("U1"), Some("Ada"), None, None),
            zilvinas,
        ]);

        assert_eq!(
            search_mention_candidates(&candidates, "ada", 10)
                .iter()
                .map(|candidate| candidate.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["U1", "U2"]
        );
        assert_eq!(
            search_mention_candidates(&candidates, "zil", 10)[0].user_id,
            "U3"
        );
    }

    #[test]
    fn mention_replacement_uses_character_offsets_and_only_appends_space_at_end() {
        let candidate = MentionCandidate {
            user_id: "U1".to_string(),
            display_name: "Zoë".to_string(),
            full_name: None,
            username: None,
            search_aliases: Vec::new(),
        };
        let text = "Živjo @zo";
        let insertion =
            replace_mention_token(text, &mention_token_at_caret(text, 9).unwrap(), &candidate);

        assert_eq!(
            insertion,
            MentionInsertion {
                text: "Živjo @Zoë ".to_string(),
                caret: 11,
                span: MentionSpan {
                    start: 6,
                    end: 10,
                    user_id: "U1".to_string(),
                    label: "@Zoë".to_string(),
                },
            }
        );

        let text = "Hi @zo!";
        let insertion =
            replace_mention_token(text, &mention_token_at_caret(text, 6).unwrap(), &candidate);
        assert_eq!(insertion.text, "Hi @Zoë!");
        assert_eq!(insertion.caret, 7);
        assert_eq!(insertion.span.start, 3);
        assert_eq!(insertion.span.end, 7);
    }

    #[test]
    fn serializes_only_exact_valid_nonoverlapping_mention_spans() {
        let text = "Hi @Ada and @Grace";
        let spans = vec![
            MentionSpan {
                start: 3,
                end: 7,
                user_id: "U1".to_string(),
                label: "@Ada".to_string(),
            },
            MentionSpan {
                start: 12,
                end: 18,
                user_id: "U2".to_string(),
                label: "@Grace".to_string(),
            },
        ];
        assert_eq!(
            serialize_composer_mentions(text, &spans),
            "Hi <@U1> and <@U2>"
        );

        let edited = vec![MentionSpan {
            start: 3,
            end: 7,
            user_id: "U1".to_string(),
            label: "@Ava".to_string(),
        }];
        assert_eq!(serialize_composer_mentions(text, &edited), text);

        let overlapping = vec![
            spans[0].clone(),
            MentionSpan {
                start: 3,
                end: 18,
                user_id: "U2".to_string(),
                label: "@Ada and @Grace".to_string(),
            },
        ];
        assert_eq!(serialize_composer_mentions(text, &overlapping), text);
    }

    #[test]
    fn hydrates_multiple_mentions_with_current_names_and_id_fallback() {
        let source = "Živjo <@U1>, meet <@U2>.";
        let names = HashMap::from([("U1".to_string(), "Zoë".to_string())]);

        let hydrated = hydrate_composer_mentions(source, &names);

        assert_eq!(hydrated.text, "Živjo @Zoë, meet @U2.");
        assert_eq!(
            hydrated.mentions,
            vec![
                MentionSpan {
                    start: 6,
                    end: 10,
                    user_id: "U1".to_string(),
                    label: "@Zoë".to_string(),
                },
                MentionSpan {
                    start: 17,
                    end: 20,
                    user_id: "U2".to_string(),
                    label: "@U2".to_string(),
                },
            ]
        );
        assert_eq!(
            serialize_composer_mentions(&hydrated.text, &hydrated.mentions),
            source
        );
    }

    #[test]
    fn hydration_preserves_malformed_or_noncanonical_mentions() {
        let source = "Keep <@>, <@U-1>, and <@U1|ada> literal";
        let hydrated = hydrate_composer_mentions(source, &HashMap::new());

        assert_eq!(hydrated.text, source);
        assert!(hydrated.mentions.is_empty());
    }

    #[test]
    fn rich_payload_preserves_unicode_styles_and_mentions() {
        let draft = RichComposerDraft {
            text: "Hi @Ada 👋".to_string(),
            mentions: vec![MentionSpan {
                start: 3,
                end: 7,
                user_id: "UADA".to_string(),
                label: "@Ada".to_string(),
            }],
            styles: vec![
                ComposerStyleSpan {
                    start: 0,
                    end: 2,
                    style: ComposerTextStyle {
                        bold: true,
                        ..Default::default()
                    },
                },
                ComposerStyleSpan {
                    start: 3,
                    end: 7,
                    style: ComposerTextStyle {
                        underline: true,
                        ..Default::default()
                    },
                },
            ],
            ..Default::default()
        };

        let payload = draft.slack_payload().expect("non-empty payload");
        let blocks: serde_json::Value =
            serde_json::from_str(&payload.blocks_json).expect("valid blocks JSON");

        assert_eq!(payload.fallback_text, "Hi <@UADA> 👋");
        assert_eq!(blocks[0]["type"], "rich_text");
        assert_eq!(blocks[0]["elements"][0]["type"], "rich_text_section");
        assert_eq!(
            blocks[0]["elements"][0]["elements"],
            serde_json::json!([
                {"type": "text", "text": "Hi", "style": {"bold": true}},
                {"type": "text", "text": " "},
                {"type": "user", "user_id": "UADA", "style": {"underline": true}},
                {"type": "text", "text": " 👋"}
            ])
        );
    }

    #[test]
    fn rich_payload_structures_lists_quotes_and_code_blocks() {
        let draft = RichComposerDraft {
            text: "one\ntwo\nquoted\nlet x = 1;\nplain".to_string(),
            blocks: vec![
                ComposerBlockSpan {
                    start: 0,
                    end: 7,
                    kind: ComposerBlockKind::BulletedList,
                },
                ComposerBlockSpan {
                    start: 8,
                    end: 14,
                    kind: ComposerBlockKind::Quote,
                },
                ComposerBlockSpan {
                    start: 15,
                    end: 25,
                    kind: ComposerBlockKind::Preformatted,
                },
            ],
            ..Default::default()
        };

        let payload = draft.slack_payload().expect("non-empty payload");
        let blocks: serde_json::Value =
            serde_json::from_str(&payload.blocks_json).expect("valid blocks JSON");
        let elements = blocks[0]["elements"].as_array().expect("rich elements");

        assert_eq!(elements[0]["type"], "rich_text_list");
        assert_eq!(elements[0]["style"], "bullet");
        assert_eq!(elements[0]["elements"].as_array().map(Vec::len), Some(2));
        assert_eq!(elements[1]["type"], "rich_text_quote");
        assert_eq!(elements[2]["type"], "rich_text_preformatted");
        assert_eq!(elements[3]["type"], "rich_text_section");
        assert_eq!(payload.fallback_text, draft.text);
    }

    #[test]
    fn rich_drafts_round_trip_and_legacy_text_remains_distinct() {
        let draft = RichComposerDraft {
            text: "Draft @Ada".to_string(),
            mentions: vec![MentionSpan {
                start: 6,
                end: 10,
                user_id: "UADA".to_string(),
                label: "@Ada".to_string(),
            }],
            entities: Vec::new(),
            styles: vec![ComposerStyleSpan {
                start: 0,
                end: 5,
                style: ComposerTextStyle {
                    italic: true,
                    ..Default::default()
                },
            }],
            blocks: vec![ComposerBlockSpan {
                start: 0,
                end: 10,
                kind: ComposerBlockKind::Quote,
            }],
            attachments: vec![ComposerAttachmentDraft {
                path: std::path::PathBuf::from("/tmp/preview.png"),
                remove_after_upload: true,
            }],
        };

        let stored = encode_rich_composer_draft(&draft);

        assert!(stored.starts_with("conduit-rich-v1:"));
        assert_eq!(decode_rich_composer_draft(&stored), Some(draft));
        assert_eq!(decode_rich_composer_draft("legacy <@UADA> draft"), None);
        assert_eq!(decode_rich_composer_draft("conduit-rich-v1:{broken"), None);
    }
}
