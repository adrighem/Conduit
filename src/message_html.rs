use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};

use crate::debug;
use crate::models::{SavedItem, SearchMatch, SlackMessage};

const MESSAGE_BASE_URI: &str = "app://conduit/messages/";

#[derive(Debug, Clone, Default)]
pub struct MessageHtmlContext {
    pub user_names: HashMap<String, String>,
    pub current_user_id: Option<String>,
    pub image_assets: HashMap<String, String>,
    pub failed_image_urls: HashSet<String>,
}

pub fn base_uri() -> &'static str {
    MESSAGE_BASE_URI
}

pub fn placeholder_document(title: &str, message: &str) -> String {
    html_document(
        title,
        &format!(
            "<main class=\"timeline\"><section class=\"placeholder\">{}</section></main>",
            escape_html(message)
        ),
    )
}

pub fn conversation_document(
    channel_id: &str,
    messages: &[SlackMessage],
    context: &MessageHtmlContext,
) -> String {
    if messages.is_empty() {
        return placeholder_document("Messages", "No messages");
    }

    let groups = message_groups(messages);
    debug::log(
        "render",
        &format!(
            "conversation channel_id={channel_id} messages={} groups={} image_assets={} failed_images={}",
            messages.len(),
            groups.len(),
            context.image_assets.len(),
            context.failed_image_urls.len()
        ),
    );

    let mut body = String::from("<main class=\"timeline\" aria-label=\"Messages\">");
    for group in groups {
        body.push_str(&message_group_article(Some(channel_id), &group, context));
    }
    body.push_str("</main>");

    html_document("Messages", &body)
}

pub fn saved_items_document(items: &[SavedItem], context: &MessageHtmlContext) -> String {
    let mut rendered = 0;
    let mut body = String::from("<main class=\"timeline\" aria-label=\"Saved items\">");

    for item in items {
        if let (Some(channel_id), Some(message)) = (item.channel.as_deref(), item.message.as_ref())
        {
            body.push_str(&message_article(Some(channel_id), message, context));
            rendered += 1;
        }
    }

    if rendered == 0 {
        body.push_str("<section class=\"placeholder\">No saved items</section>");
    }
    body.push_str("</main>");

    html_document("Saved items", &body)
}

pub fn search_results_document(results: &[SearchMatch], context: &MessageHtmlContext) -> String {
    if results.is_empty() {
        return placeholder_document("Search results", "No results");
    }

    let mut body = String::from("<main class=\"timeline\" aria-label=\"Search results\">");
    for result in results {
        body.push_str(&search_result_article(result, context));
    }
    body.push_str("</main>");

    html_document("Search results", &body)
}

fn html_document(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>
:root {{
  color-scheme: light dark;
  --page: #fafafa;
  --text: #202124;
  --muted: #6a6f76;
  --line: #deddda;
  --soft: #f1f3f4;
  --code: #eceff1;
  --accent: #0a7cff;
  --accent-soft: #e5f1ff;
  --success-soft: #e7f4e8;
}}

@media (prefers-color-scheme: dark) {{
  :root {{
    --page: #1d1f20;
    --text: #f2f2f2;
    --muted: #b6babf;
    --line: #3b3f42;
    --soft: #2a2d2f;
    --code: #303437;
    --accent: #78aeff;
    --accent-soft: #183653;
    --success-soft: #203827;
  }}
}}

html, body {{
  min-height: 100%;
  margin: 0;
  background: var(--page);
  color: var(--text);
  font: 14px/1.45 Cantarell, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}}

body {{
  overflow-wrap: anywhere;
}}

a {{
  color: var(--accent);
  text-decoration: none;
}}

a:hover {{
  text-decoration: underline;
}}

.timeline {{
  box-sizing: border-box;
  width: 100%;
  max-width: 880px;
  margin: 0 auto;
  padding: 4px 12px 20px;
}}

.message {{
  display: grid;
  gap: 6px;
  padding: 10px 0;
  border-bottom: 1px solid var(--line);
}}

.message-stack {{
  display: grid;
  gap: 8px;
}}

.message-part {{
  display: grid;
  gap: 6px;
}}

.message-part + .message-part {{
  padding-top: 4px;
}}

.message-header {{
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
}}

.author {{
  min-width: 0;
  font-weight: 700;
}}

.metadata {{
  color: var(--muted);
  font-size: 12px;
}}

.body {{
  min-width: 0;
}}

.body p {{
  margin: 0;
}}

.context-block,
.empty-message,
.attachment,
.image-alt {{
  color: var(--muted);
}}

.divider {{
  height: 1px;
  border: 0;
  background: var(--line);
  margin: 4px 0;
}}

code {{
  padding: 1px 4px;
  border-radius: 4px;
  background: var(--code);
  font-family: ui-monospace, "Cascadia Mono", "SF Mono", Menlo, Consolas, monospace;
  font-size: 13px;
}}

pre {{
  margin: 2px 0;
  padding: 10px;
  overflow-x: auto;
  border-radius: 8px;
  background: var(--code);
}}

pre code {{
  padding: 0;
  border-radius: 0;
  background: transparent;
  font-size: 13px;
}}

.mention {{
  display: inline-block;
  padding: 0 4px;
  border-radius: 4px;
  background: var(--accent-soft);
  font-weight: 700;
}}

.emoji {{
  font-family: "Noto Color Emoji", "Apple Color Emoji", "Segoe UI Emoji", sans-serif;
  line-height: 1;
}}

.attachments,
.reactions,
.message-actions,
.block-actions {{
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
}}

.attachment,
.block-action {{
  padding: 3px 7px;
  border-radius: 6px;
  background: var(--soft);
}}

.image-attachment {{
  display: grid;
  gap: 6px;
  max-width: 520px;
  margin: 2px 0;
}}

.image-attachment img {{
  display: block;
  width: auto;
  max-width: 100%;
  max-height: 420px;
  border-radius: 8px;
  background: var(--soft);
}}

.image-caption {{
  color: var(--muted);
  font-size: 12px;
}}

.image-placeholder {{
  display: flex;
  align-items: center;
  min-height: 72px;
  padding: 10px;
  border-radius: 8px;
  background: var(--soft);
  color: var(--muted);
}}

.reaction {{
  padding: 2px 7px;
  border-radius: 999px;
  background: var(--soft);
  color: var(--muted);
  font-size: 12px;
}}

.reaction.is-active {{
  background: var(--success-soft);
  color: var(--text);
  font-weight: 700;
}}

.action {{
  display: inline-flex;
  align-items: center;
  min-height: 26px;
  padding: 0 10px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--soft);
  color: var(--text);
  font-weight: 600;
  font-size: 12px;
}}

.action:hover {{
  border-color: var(--accent);
  text-decoration: none;
}}

.action.is-active {{
  background: var(--success-soft);
}}

.image-link {{
  color: inherit;
  text-decoration: none;
}}

.image-link:hover {{
  text-decoration: none;
}}

.placeholder {{
  padding: 14px 0;
  color: var(--muted);
}}
</style>
</head>
<body>
{}
</body>
</html>"#,
        escape_html(title),
        body
    )
}

fn message_article(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let author = author_label(message, context);
    let mut article = format!(
        "<article class=\"message\"><header class=\"message-header\"><span class=\"author\">{}</span>{}</header><div class=\"body\">{}</div>",
        escape_html(&author),
        metadata_html(message),
        message_body_html(message, context)
    );

    article.push_str(&attachments_html(message, context));
    article.push_str(&reactions_html(message, context));
    article.push_str(&message_actions_html(channel_id, message, context));
    article.push_str("</article>");
    article
}

fn message_group_article(
    channel_id: Option<&str>,
    messages: &[&SlackMessage],
    context: &MessageHtmlContext,
) -> String {
    let Some(first_message) = messages.first() else {
        return String::new();
    };

    let author = author_label(first_message, context);
    let mut article = format!(
        "<article class=\"message message-group\"><header class=\"message-header\"><span class=\"author\">{}</span>{}</header><div class=\"message-stack\">",
        escape_html(&author),
        metadata_html(first_message)
    );

    for message in messages {
        article.push_str(&message_part_html(channel_id, message, context));
    }

    article.push_str("</div></article>");
    article
}

fn message_part_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let mut part = format!(
        "<section class=\"message-part\"><div class=\"body\">{}</div>",
        message_body_html(message, context)
    );

    part.push_str(&attachments_html(message, context));
    part.push_str(&reactions_html(message, context));
    part.push_str(&message_actions_html(channel_id, message, context));
    part.push_str("</section>");
    part
}

fn message_groups(messages: &[SlackMessage]) -> Vec<Vec<&SlackMessage>> {
    let ordered = messages.iter().rev().collect::<Vec<_>>();
    let mut groups: Vec<Vec<&SlackMessage>> = Vec::new();

    for message in ordered {
        if let Some(group) = groups.last_mut() {
            if group
                .last()
                .is_some_and(|previous| can_group_messages(previous, message))
            {
                group.push(message);
                continue;
            }
        }

        groups.push(vec![message]);
    }

    groups
}

fn can_group_messages(previous: &SlackMessage, current: &SlackMessage) -> bool {
    if sender_key(previous) != sender_key(current) {
        return false;
    }

    let Some(previous_ts) = slack_ts_seconds(&previous.ts) else {
        return false;
    };
    let Some(current_ts) = slack_ts_seconds(&current.ts) else {
        return false;
    };
    let delta = current_ts - previous_ts;

    (0.0..180.0).contains(&delta)
}

fn sender_key(message: &SlackMessage) -> String {
    message
        .user
        .as_deref()
        .or(message.username.as_deref())
        .unwrap_or("Slack")
        .to_string()
}

fn slack_ts_seconds(ts: &str) -> Option<f64> {
    ts.parse::<f64>().ok()
}

fn search_result_article(result: &SearchMatch, context: &MessageHtmlContext) -> String {
    let channel = result
        .channel
        .as_ref()
        .and_then(|channel| channel.name.as_deref())
        .map(|name| format!("#{name}"))
        .unwrap_or_else(|| "Slack".to_string());
    let author = result
        .username
        .as_deref()
        .or(result.user.as_deref())
        .unwrap_or("Unknown");
    let text = result.text.as_deref().unwrap_or_default();

    let mut article = format!(
        "<article class=\"message\"><header class=\"message-header\"><span class=\"author\">{}</span><span class=\"metadata\">{}</span></header><div class=\"body\"><p>{}</p></div>",
        escape_html(author),
        escape_html(&channel),
        mrkdwn_to_html(text, context)
    );

    if let Some(permalink) = result.permalink.as_deref().filter(|url| is_http_url(url)) {
        article.push_str(&format!(
            "<nav class=\"message-actions\"><a class=\"action\" href=\"{}\" rel=\"noreferrer noopener\">Open in Slack</a></nav>",
            escape_html(permalink)
        ));
    }

    article.push_str("</article>");
    article
}

fn metadata_html(message: &SlackMessage) -> String {
    let mut metadata = timestamp_html(&message.ts);

    match message.subtype.as_deref() {
        Some(subtype) if !subtype.is_empty() => {
            metadata.push_str(&format!(
                "<span class=\"metadata\">{}</span>",
                escape_html(&subtype.replace('_', " "))
            ));
        }
        _ => {}
    }

    metadata
}

fn timestamp_html(ts: &str) -> String {
    let Some(datetime) = slack_ts_datetime(ts) else {
        return String::new();
    };

    let short = datetime.format("%H:%M").to_string();
    let full = datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string();
    format!(
        "<time class=\"metadata\" datetime=\"{}\" title=\"{}\">{}</time>",
        escape_html(&datetime.to_rfc3339()),
        escape_html(&full),
        escape_html(&short)
    )
}

fn slack_ts_datetime(ts: &str) -> Option<DateTime<Local>> {
    let (seconds, nanos) = parse_slack_ts(ts)?;
    DateTime::from_timestamp(seconds, nanos).map(|datetime| datetime.with_timezone(&Local))
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

fn author_label(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    message
        .user
        .as_ref()
        .and_then(|user_id| context.user_names.get(user_id))
        .cloned()
        .unwrap_or_else(|| message.author_label())
}

fn message_body_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    if let Some(blocks) = message.blocks.as_ref() {
        let rendered = blocks_html(blocks, context);
        if !rendered.is_empty() {
            return rendered;
        }
    }

    let text = message.body_text();
    if text.trim().is_empty() {
        "<p class=\"empty-message\">No message text</p>".to_string()
    } else {
        text_block_html(&text, None, context)
    }
}

fn blocks_html(blocks: &serde_json::Value, context: &MessageHtmlContext) -> String {
    let Some(blocks) = blocks.as_array() else {
        return String::new();
    };

    let mut rendered = String::new();
    for block in blocks {
        let Some(kind) = block.get("type").and_then(|kind| kind.as_str()) else {
            continue;
        };

        match kind {
            "section" => {
                if let Some(text) = block_text(block) {
                    rendered.push_str(&text_block_html(&text, None, context));
                }
            }
            "context" => {
                let text = block
                    .get("elements")
                    .and_then(|elements| elements.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(block_text)
                    .collect::<Vec<_>>()
                    .join("  ");
                if !text.is_empty() {
                    rendered.push_str(&text_block_html(&text, Some("context-block"), context));
                }
            }
            "divider" => rendered.push_str("<hr class=\"divider\">"),
            "image" => {
                let alt = block
                    .get("alt_text")
                    .and_then(|text| text.as_str())
                    .unwrap_or("Image");
                if let Some(url) = block
                    .get("image_url")
                    .and_then(|url| url.as_str())
                    .filter(|url| is_http_url(url))
                {
                    rendered.push_str(&image_figure_html(
                        url,
                        Some(url),
                        alt,
                        Some("Slack image"),
                        context,
                    ));
                } else {
                    rendered.push_str(&format!(
                        "<p class=\"image-alt\">Image: {}</p>",
                        escape_html(alt)
                    ));
                }
            }
            "actions" => {
                let labels = block
                    .get("elements")
                    .and_then(|elements| elements.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(block_text)
                    .map(|label| {
                        format!(
                            "<span class=\"block-action\">{}</span>",
                            mrkdwn_to_html(&label, context)
                        )
                    })
                    .collect::<String>();
                if !labels.is_empty() {
                    rendered.push_str(&format!("<div class=\"block-actions\">{labels}</div>"));
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

fn text_block_html(text: &str, class_name: Option<&str>, context: &MessageHtmlContext) -> String {
    let html = mrkdwn_to_html(text, context);
    let class = class_name
        .map(|class_name| format!(" class=\"{class_name}\""))
        .unwrap_or_default();

    if html.contains("<pre>") {
        format!("<div{class}>{html}</div>")
    } else {
        format!("<p{class}>{html}</p>")
    }
}

fn attachments_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    let Some(files) = message.files.as_ref().filter(|files| !files.is_empty()) else {
        return String::new();
    };

    let attachments = files
        .iter()
        .map(|file| {
            let label = file.display_title();
            if file.is_image() {
                if let Some(url) = file.preview_url() {
                    return image_figure_html(
                        url,
                        file.permalink.as_deref().or(file.url_private.as_deref()),
                        label,
                        Some(label),
                        context,
                    );
                }
            }

            attachment_chip_html(
                label,
                file.permalink.as_deref().or(file.url_private.as_deref()),
            )
        })
        .collect::<String>();

    format!("<div class=\"attachments\">{attachments}</div>")
}

fn attachment_chip_html(label: &str, link: Option<&str>) -> String {
    let label = format!("Attachment: {}", escape_html(label));
    if let Some(link) = link.filter(|link| is_http_url(link)) {
        format!(
            "<a class=\"attachment\" href=\"{}\" rel=\"noreferrer noopener\">{label}</a>",
            escape_html(link)
        )
    } else {
        format!("<span class=\"attachment\">{label}</span>")
    }
}

fn image_figure_html(
    asset_key: &str,
    link: Option<&str>,
    alt: &str,
    caption: Option<&str>,
    context: &MessageHtmlContext,
) -> String {
    let image = if let Some(src) = context.image_assets.get(asset_key) {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=loaded key={}", debug::url_for_log(asset_key)),
            );
        }
        format!(
            "<img loading=\"lazy\" decoding=\"async\" src=\"{}\" alt=\"{}\">",
            escape_html(src),
            escape_html(alt)
        )
    } else if context.failed_image_urls.contains(asset_key) {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=failed key={}", debug::url_for_log(asset_key)),
            );
        }
        "<div class=\"image-placeholder\">Image preview unavailable</div>".to_string()
    } else if is_http_url(asset_key) && !requires_authenticated_image(asset_key) {
        if debug::enabled() {
            debug::log(
                "render",
                &format!(
                    "image state=direct-webkit key={}",
                    debug::url_for_log(asset_key)
                ),
            );
        }
        format!(
            "<img loading=\"lazy\" decoding=\"async\" src=\"{}\" alt=\"{}\">",
            escape_html(asset_key),
            escape_html(alt)
        )
    } else {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=pending key={}", debug::url_for_log(asset_key)),
            );
        }
        "<div class=\"image-placeholder\">Loading image preview</div>".to_string()
    };

    let caption = caption
        .filter(|caption| !caption.trim().is_empty())
        .map(|caption| {
            format!(
                "<figcaption class=\"image-caption\">{}</figcaption>",
                escape_html(caption)
            )
        })
        .unwrap_or_default();

    let figure = format!("<figure class=\"image-attachment\">{image}{caption}</figure>");
    if let Some(link) = link.filter(|link| is_http_url(link)) {
        format!(
            "<a class=\"image-link\" href=\"{}\" rel=\"noreferrer noopener\">{figure}</a>",
            escape_html(link)
        )
    } else {
        figure
    }
}

fn reactions_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    let Some(reactions) = message
        .reactions
        .as_ref()
        .filter(|reactions| !reactions.is_empty())
    else {
        return String::new();
    };

    let reactions = reactions
        .iter()
        .filter_map(|reaction| {
            let name = reaction.name.as_deref()?;
            let count = reaction.count.unwrap_or_default();
            let active = context.current_user_id.as_ref().is_some_and(|user_id| {
                reaction
                    .users
                    .as_ref()
                    .is_some_and(|users| users.iter().any(|user| user == user_id))
            });
            let active_class = if active { " is-active" } else { "" };
            Some(format!(
                "<span class=\"reaction{}\">{} {}</span>",
                active_class,
                escape_html(&reaction_label(name)),
                count
            ))
        })
        .collect::<String>();

    if reactions.is_empty() {
        String::new()
    } else {
        format!("<div class=\"reactions\">{reactions}</div>")
    }
}

fn message_actions_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let Some(channel_id) = channel_id else {
        return String::new();
    };
    if message.ts.is_empty() {
        return String::new();
    }

    let mut actions = String::new();
    let reacted = message.user_reacted("thumbsup", context.current_user_id.as_deref());
    let reaction_label = if reacted { "Remove +1" } else { "+1" };
    let active_class = if reacted { " is-active" } else { "" };
    actions.push_str(&format!(
        "<a class=\"action{}\" href=\"{}\">{}</a>",
        active_class,
        escape_html(&reaction_action_url(channel_id, message, !reacted)),
        reaction_label
    ));

    if message.has_thread() {
        let label = message
            .reply_count
            .filter(|count| *count > 0)
            .map(|count| format!("View thread ({count})"))
            .unwrap_or_else(|| "View thread".to_string());
        actions.push_str(&format!(
            "<a class=\"action\" href=\"{}\">{}</a>",
            escape_html(&thread_action_url(channel_id, &message.ts)),
            escape_html(&label)
        ));
    }

    format!("<nav class=\"message-actions\">{actions}</nav>")
}

pub fn thread_action_url(channel_id: &str, ts: &str) -> String {
    format!(
        "conduit://thread?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(ts)
    )
}

pub fn reaction_action_url(channel_id: &str, message: &SlackMessage, add: bool) -> String {
    let mut url = format!(
        "conduit://reaction?channel={}&ts={}&name=thumbsup&add={}",
        encode_query(channel_id),
        encode_query(&message.ts),
        add
    );

    if let Some(thread_ts) = message.thread_ts.as_deref().filter(|ts| !ts.is_empty()) {
        url.push_str("&thread_ts=");
        url.push_str(&encode_query(thread_ts));
    }

    url
}

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn reaction_label(name: &str) -> String {
    emoji_for_code(name)
        .or_else(|| match name {
            "+1" => Some("👍"),
            "-1" => Some("👎"),
            _ => None,
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| format!(":{name}:"))
}

fn mrkdwn_to_html(text: &str, context: &MessageHtmlContext) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        output.push_str(&render_inline(&rest[..start], context));
        rest = &rest[start + 3..];
        if let Some(end) = rest.find("```") {
            output.push_str("<pre><code>");
            output.push_str(&escape_html(&rest[..end]));
            output.push_str("</code></pre>");
            rest = &rest[end + 3..];
        } else {
            output.push_str(&escape_html("```"));
            output.push_str(&render_inline(rest, context));
            rest = "";
        }
    }

    output.push_str(&render_inline(rest, context));
    output
}

fn render_inline(text: &str, context: &MessageHtmlContext) -> String {
    let mut output = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                output.push_str("<code>");
                output.push_str(&escape_html(&rest[1..1 + end]));
                output.push_str("</code>");
                rest = &rest[end + 2..];
                continue;
            }
        }

        if let Some((html, consumed)) = render_slack_entity(rest, context) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((html, consumed)) = render_emoji_shortcode(rest) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((html, consumed)) = render_wrapped(rest, '*', "strong", context) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((html, consumed)) = render_wrapped(rest, '_', "em", context) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }

        if let Some((html, consumed)) = render_wrapped(rest, '~', "s", context) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }

        let next = rest.chars().next().expect("non-empty string has a char");
        if next == '\n' {
            output.push_str("<br>");
        } else {
            output.push_str(&escape_html(&next.to_string()));
        }
        rest = &rest[next.len_utf8()..];
    }

    output
}

fn render_slack_entity(text: &str, context: &MessageHtmlContext) -> Option<(String, usize)> {
    if !text.starts_with('<') {
        return None;
    }

    let end = text.find('>')?;
    let raw = &text[1..end];
    let rendered = if let Some(user_id) = raw.strip_prefix('@') {
        let name = context
            .user_names
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string());
        format!("<span class=\"mention\">@{}</span>", escape_html(&name))
    } else if let Some(channel) = raw.strip_prefix('#') {
        let display = channel
            .split_once('|')
            .map(|(_, label)| format!("#{label}"))
            .unwrap_or_else(|| format!("#{channel}"));
        escape_html(&display)
    } else if let Some((url, label)) = raw.split_once('|') {
        if is_http_url(url) {
            external_link_html(url, label)
        } else {
            escape_html(label)
        }
    } else if raw.starts_with('!') {
        slack_special_entity_html(raw)
    } else if is_http_url(raw) {
        external_link_html(raw, raw)
    } else {
        return None;
    };

    Some((rendered, end + 1))
}

fn slack_special_entity_html(raw: &str) -> String {
    if let Some((_, label)) = raw.rsplit_once('|') {
        return escape_html(label);
    }

    match raw {
        "!channel" => "@channel".to_string(),
        "!here" => "@here".to_string(),
        "!everyone" => "@everyone".to_string(),
        _ => escape_html(raw),
    }
}

fn render_emoji_shortcode(text: &str) -> Option<(String, usize)> {
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

    let shortcode = &text[..end + 1];
    let emoji = emoji_for_code(code);
    if debug::enabled() {
        debug::log(
            "render",
            &format!("emoji shortcode=:{code}: mapped={}", emoji.is_some()),
        );
    }

    let rendered = emoji
        .map(|emoji| {
            format!(
                "<span class=\"emoji\" title=\":{}:\">{}</span>",
                escape_html(code),
                emoji
            )
        })
        .unwrap_or_else(|| escape_html(shortcode));

    Some((rendered, end + 1))
}

fn emoji_for_code(code: &str) -> Option<&'static str> {
    match code {
        "+1" | "thumbsup" => Some("👍"),
        "-1" | "thumbsdown" => Some("👎"),
        "clap" => Some("👏"),
        "eyes" => Some("👀"),
        "fire" => Some("🔥"),
        "heart" => Some("❤️"),
        "heart_eyes" => Some("😍"),
        "joy" => Some("😂"),
        "laughing" | "satisfied" => Some("😆"),
        "ok_hand" => Some("👌"),
        "party_parrot" | "tada" => Some("🎉"),
        "pray" => Some("🙏"),
        "rocket" => Some("🚀"),
        "sad" => Some("😢"),
        "slightly_smiling_face" | "simple_smile" | "smile" => Some("🙂"),
        "smiley" => Some("😃"),
        "stuck_out_tongue" | "face_with_tongue" => Some("😛"),
        "stuck_out_tongue_closed_eyes" => Some("😝"),
        "stuck_out_tongue_winking_eye" => Some("😜"),
        "sweat_smile" => Some("😅"),
        "thinking_face" => Some("🤔"),
        "white_check_mark" => Some("✅"),
        "yum" => Some("😋"),
        "zany_face" => Some("🤪"),
        "x" => Some("❌"),
        _ => None,
    }
}

fn render_wrapped(
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
        format!("<{tag}>{}</{tag}>", render_inline(inner, context)),
        marker_len + end + marker_len,
    ))
}

fn external_link_html(url: &str, label: &str) -> String {
    format!(
        "<a href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
        escape_html(url),
        escape_html(label)
    )
}

fn is_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn requires_authenticated_image(value: &str) -> bool {
    url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host == "files.slack.com" || host.ends_with(".slack-files.com"))
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SavedItem, SlackFile, SlackReaction};

    fn message(text: &str) -> SlackMessage {
        SlackMessage {
            user: Some("U123".to_string()),
            text: Some(text.to_string()),
            ts: "1710000000.000100".to_string(),
            ..Default::default()
        }
    }

    fn message_at(user: &str, text: &str, ts: &str) -> SlackMessage {
        SlackMessage {
            user: Some(user.to_string()),
            text: Some(text.to_string()),
            ts: ts.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn escapes_message_text_and_author() {
        let mut message = message("hello <script>alert(1)</script> & goodbye");
        message.username = Some("<bad author>".to_string());

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("hello &lt;script&gt;alert(1)&lt;/script&gt; &amp; goodbye"));
        assert!(html.contains("&lt;bad author&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn resolves_mentions_channels_and_slack_links() {
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U123".to_string(), "Ada".to_string())]),
            current_user_id: None,
            ..Default::default()
        };
        let html = conversation_document(
            "C123",
            &[message(
                "hi <@U123> in <#C999|general> see <https://example.com|docs>",
            )],
            &context,
        );

        assert!(html.contains("<span class=\"mention\">@Ada</span>"));
        assert!(html.contains("#general"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains(">docs</a>"));
    }

    #[test]
    fn renders_common_slack_emoji_shortcodes() {
        let html = conversation_document(
            "C123",
            &[message(
                "ship it :rocket: :stuck_out_tongue: :unknown_custom_emoji:",
            )],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains("<span class=\"emoji\" title=\":rocket:\">🚀</span>"));
        assert!(html.contains("<span class=\"emoji\" title=\":stuck_out_tongue:\">😛</span>"));
        assert!(html.contains(":unknown_custom_emoji:"));
    }

    #[test]
    fn renders_message_timestamp_with_full_datetime_tooltip() {
        let html =
            conversation_document("C123", &[message("timed")], &MessageHtmlContext::default());

        assert!(html.contains("<time class=\"metadata\""));
        assert!(html.contains("datetime=\""));
        assert!(html.contains("title=\""));
        assert!(html.contains("</time>"));
    }

    #[test]
    fn groups_adjacent_messages_from_same_sender_under_three_minutes() {
        let messages = vec![
            message_at("U999", "other", "1710000300.000000"),
            message_at("U123", "second", "1710000100.000000"),
            message_at("U123", "first", "1710000000.000000"),
        ];

        let html = conversation_document("C123", &messages, &MessageHtmlContext::default());

        assert_eq!(html.matches("message-group").count(), 2);
        assert!(html.contains("first"));
        assert!(html.contains("second"));
    }

    #[test]
    fn does_not_group_same_sender_after_three_minutes() {
        let messages = vec![
            message_at("U123", "later", "1710000180.000000"),
            message_at("U123", "first", "1710000000.000000"),
        ];

        let html = conversation_document("C123", &messages, &MessageHtmlContext::default());

        assert_eq!(html.matches("message-group").count(), 2);
    }

    #[test]
    fn renders_code_blocks_as_escaped_preformatted_html() {
        let html = conversation_document(
            "C123",
            &[message("```<b>not bold</b>```")],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains("<pre><code>&lt;b&gt;not bold&lt;/b&gt;</code></pre>"));
        assert!(!html.contains("<p><pre>"));
    }

    #[test]
    fn renders_interactive_thread_and_reaction_actions() {
        let mut message = message("threaded");
        message.reply_count = Some(3);
        message.reactions = Some(vec![SlackReaction {
            name: Some("thumbsup".to_string()),
            count: Some(1),
            users: Some(vec!["U999".to_string()]),
        }]);
        let context = MessageHtmlContext {
            current_user_id: Some("U999".to_string()),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("conduit://thread?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=thumbsup&amp;add=false"));
        assert!(html.contains("Remove +1"));
        assert!(html.contains("View thread (3)"));
    }

    #[test]
    fn renders_lazy_image_attachment_from_loaded_asset() {
        let image_url = "https://files.slack.com/files-pri/T123-F123/image.png";
        let mut message = message("image");
        message.files = Some(vec![SlackFile {
            title: Some("Diagram".to_string()),
            mimetype: Some("image/png".to_string()),
            thumb_480: Some(image_url.to_string()),
            ..Default::default()
        }]);
        let context = MessageHtmlContext {
            image_assets: HashMap::from([(
                image_url.to_string(),
                "data:image/png;base64,abc".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("loading=\"lazy\""));
        assert!(html.contains("src=\"data:image/png;base64,abc\""));
        assert!(html.contains("Diagram"));
    }

    #[test]
    fn renders_image_attachment_placeholder_while_asset_loads() {
        let mut message = message("image");
        message.files = Some(vec![SlackFile {
            title: Some("Diagram".to_string()),
            mimetype: Some("image/png".to_string()),
            thumb_480: Some("https://files.slack.com/files-pri/T123-F123/image.png".to_string()),
            ..Default::default()
        }]);

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("Loading image preview"));
    }

    #[test]
    fn saved_items_ignore_non_message_entries() {
        let items = vec![
            SavedItem {
                kind: Some("file".to_string()),
                ..Default::default()
            },
            SavedItem {
                channel: Some("C123".to_string()),
                message: Some(message("saved")),
                ..Default::default()
            },
        ];

        let html = saved_items_document(&items, &MessageHtmlContext::default());

        assert!(html.contains("saved"));
        assert!(!html.contains("No saved items"));
    }

    #[test]
    fn search_results_do_not_link_unsafe_permalink_schemes() {
        let results = vec![SearchMatch {
            username: Some("Ada".to_string()),
            text: Some("result".to_string()),
            permalink: Some("javascript:alert(1)".to_string()),
            ..Default::default()
        }];

        let html = search_results_document(&results, &MessageHtmlContext::default());

        assert!(html.contains("result"));
        assert!(!html.contains("javascript:alert"));
        assert!(!html.contains("Open in Slack"));
    }
}
