use std::collections::HashMap;

use gtk::glib;
use gtk::prelude::*;

use crate::models::SlackMessage;

pub fn append_message_content(
    container: &gtk::Box,
    message: &SlackMessage,
    user_names: &HashMap<String, String>,
) {
    if let Some(blocks) = message.blocks.as_ref() {
        if append_blocks(container, blocks, user_names) {
            return;
        }
    }

    container.append(&rich_label(&message.body_text(), user_names));
}

pub fn extract_user_ids(message: &SlackMessage) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(user) = message.user.as_ref() {
        ids.push(user.clone());
    }
    extract_mentions(&message.body_text(), &mut ids);
    ids.sort();
    ids.dedup();
    ids
}

pub fn rich_label(text: &str, user_names: &HashMap<String, String>) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true);
    label.set_use_markup(true);
    label.set_markup(&mrkdwn_to_pango(text, user_names));
    label
}

pub fn mrkdwn_to_pango(text: &str, user_names: &HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        output.push_str(&render_inline(&rest[..start], user_names));
        rest = &rest[start + 3..];
        if let Some(end) = rest.find("```") {
            output.push_str("<span font_family=\"monospace\">");
            output.push_str(&escape(&rest[..end]));
            output.push_str("</span>");
            rest = &rest[end + 3..];
        } else {
            output.push_str(&escape("```"));
            output.push_str(&render_inline(rest, user_names));
            rest = "";
        }
    }

    output.push_str(&render_inline(rest, user_names));
    output
}

fn append_blocks(
    container: &gtk::Box,
    blocks: &serde_json::Value,
    user_names: &HashMap<String, String>,
) -> bool {
    let Some(blocks) = blocks.as_array() else {
        return false;
    };

    let mut rendered = false;
    for block in blocks {
        let Some(kind) = block.get("type").and_then(|kind| kind.as_str()) else {
            continue;
        };

        match kind {
            "section" => {
                if let Some(text) = block_text(block) {
                    container.append(&rich_label(&text, user_names));
                    rendered = true;
                }
            }
            "context" => {
                if let Some(elements) = block
                    .get("elements")
                    .and_then(|elements| elements.as_array())
                {
                    let text = elements
                        .iter()
                        .filter_map(block_text)
                        .collect::<Vec<_>>()
                        .join("  ");
                    if !text.is_empty() {
                        let label = rich_label(&text, user_names);
                        label.add_css_class("caption");
                        container.append(&label);
                        rendered = true;
                    }
                }
            }
            "divider" => {
                container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                rendered = true;
            }
            "image" => {
                let alt = block
                    .get("alt_text")
                    .and_then(|text| text.as_str())
                    .unwrap_or("Image");
                let label = gtk::Label::new(Some(&format!("Image: {alt}")));
                label.set_xalign(0.0);
                label.add_css_class("caption");
                container.append(&label);
                rendered = true;
            }
            "actions" => {
                if let Some(elements) = block
                    .get("elements")
                    .and_then(|elements| elements.as_array())
                {
                    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                    for element in elements {
                        if let Some(label) = block_text(element) {
                            let button = gtk::Button::with_label(&label);
                            button.set_sensitive(false);
                            actions.append(&button);
                        }
                    }
                    container.append(&actions);
                    rendered = true;
                }
            }
            _ => {}
        }
    }

    rendered
}

fn block_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("text").and_then(|text| text.as_str()) {
        return Some(text.to_string());
    }

    value
        .get("text")
        .and_then(|text| text.get("text"))
        .and_then(|text| text.as_str())
        .map(ToString::to_string)
}

fn render_inline(text: &str, user_names: &HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut index = 0;
    let bytes = text.as_bytes();

    while index < text.len() {
        let rest = &text[index..];

        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                output.push_str("<span font_family=\"monospace\">");
                output.push_str(&escape(&rest[1..1 + end]));
                output.push_str("</span>");
                index += end + 2;
                continue;
            }
        }

        if let Some((markup, consumed)) = render_slack_entity(rest, user_names) {
            output.push_str(&markup);
            index += consumed;
            continue;
        }

        if let Some((tag, consumed)) = render_wrapped(rest, '*', "b", user_names) {
            output.push_str(&tag);
            index += consumed;
            continue;
        }

        if let Some((tag, consumed)) = render_wrapped(rest, '_', "i", user_names) {
            output.push_str(&tag);
            index += consumed;
            continue;
        }

        if let Some((tag, consumed)) = render_wrapped(rest, '~', "s", user_names) {
            output.push_str(&tag);
            index += consumed;
            continue;
        }

        let next = next_char_len(bytes[index]);
        output.push_str(&escape(&text[index..index + next]));
        index += next;
    }

    output
}

fn render_slack_entity(
    text: &str,
    user_names: &HashMap<String, String>,
) -> Option<(String, usize)> {
    if !text.starts_with('<') {
        return None;
    }

    let end = text.find('>')?;
    let raw = &text[1..end];
    let rendered = if let Some(user_id) = raw.strip_prefix('@') {
        let name = user_names
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string());
        format!("<b>@{}</b>", escape(&name))
    } else if let Some(channel) = raw.strip_prefix('#') {
        let display = channel
            .split_once('|')
            .map(|(_, label)| format!("#{label}"))
            .unwrap_or_else(|| format!("#{channel}"));
        escape(&display).to_string()
    } else if let Some((_, label)) = raw.split_once('|') {
        format!("<u>{}</u>", escape(label))
    } else {
        format!("<u>{}</u>", escape(raw))
    };

    Some((rendered, end + 1))
}

fn render_wrapped(
    text: &str,
    marker: char,
    tag: &str,
    user_names: &HashMap<String, String>,
) -> Option<(String, usize)> {
    if !text.starts_with(marker) {
        return None;
    }

    let end = text[1..].find(marker)?;
    let inner = &text[1..1 + end];
    if inner.trim().is_empty() {
        return None;
    }

    Some((
        format!("<{tag}>{}</{tag}>", render_inline(inner, user_names)),
        end + 2,
    ))
}

fn extract_mentions(text: &str, ids: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            return;
        };
        ids.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
}

fn escape(text: &str) -> glib::GString {
    glib::markup_escape_text(text)
}

fn next_char_len(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte < 0xE0 {
        2
    } else if byte < 0xF0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_plain_text() {
        assert_eq!(mrkdwn_to_pango("a & b", &HashMap::new()), "a &amp; b");
    }

    #[test]
    fn renders_basic_inline_markup() {
        assert_eq!(
            mrkdwn_to_pango("*bold* _italic_ ~done~ `code`", &HashMap::new()),
            "<b>bold</b> <i>italic</i> <s>done</s> <span font_family=\"monospace\">code</span>"
        );
    }

    #[test]
    fn resolves_known_mentions() {
        let names = HashMap::from([("U123".to_string(), "Ada".to_string())]);
        assert_eq!(mrkdwn_to_pango("hi <@U123>", &names), "hi <b>@Ada</b>");
    }
}
