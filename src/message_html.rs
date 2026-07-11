use std::collections::{HashMap, HashSet};

use gettextrs::gettext;

use crate::activity::ActivityItem;
use crate::debug;
use crate::emoji::{EmojiCatalog, EmojiEntry, EmojiValue};
use crate::models::{SavedItem, SearchMatch, SearchMessageLocation, SlackFile, SlackMessage};

const MESSAGE_BASE_URI: &str = "app://conduit/messages/";
const DEFAULT_DOCUMENT_LANGUAGE: &str = "en";

#[derive(Debug, Clone, Default)]
pub struct MessageHtmlContext {
    pub user_names: HashMap<String, String>,
    pub user_group_names: HashMap<String, String>,
    pub user_group_members: HashMap<String, Vec<String>>,
    pub current_user_id: Option<String>,
    pub thread_ts: Option<String>,
    pub load_more_url: Option<String>,
    pub timeline_scroll: TimelineScrollBehavior,
    pub image_assets: HashMap<String, String>,
    pub failed_image_urls: HashSet<String>,
    pub recent_reactions: Vec<String>,
    pub custom_emojis: HashMap<String, String>,
    pub read_marker_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineScrollBehavior {
    #[default]
    Preserve,
    PreservePrepend,
    Bottom,
    StickToBottom,
}

impl TimelineScrollBehavior {
    fn js_mode(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::PreservePrepend => "preserve-prepend",
            Self::Bottom => "bottom",
            Self::StickToBottom => "stick-to-bottom",
        }
    }
}

pub fn base_uri() -> &'static str {
    MESSAGE_BASE_URI
}

fn normalize_language_tag(locale: &str) -> Option<String> {
    let locale = locale.trim();
    let (locale, modifier) = locale
        .split_once('@')
        .map_or((locale, None), |(locale, modifier)| {
            (locale, Some(modifier.to_ascii_lowercase()))
        });
    let locale = locale.split('.').next().unwrap_or_default();
    if locale.is_empty() || matches!(locale, "C" | "POSIX") {
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
        Some("latin") => Some("Latn"),
        Some("cyrillic") => Some("Cyrl"),
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

    Some(normalized.join("-"))
}

fn document_language() -> String {
    gtk::glib::language_names()
        .iter()
        .find_map(|language| normalize_language_tag(language.as_str()))
        .unwrap_or_else(|| DEFAULT_DOCUMENT_LANGUAGE.to_string())
}

fn document_heading(title: &str) -> String {
    format!(
        "<h1 id=\"document-title\" class=\"visually-hidden\">{}</h1>",
        escape_html(title)
    )
}

fn reaction_picker_html(context: &MessageHtmlContext) -> String {
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let entries = catalog.entries();
    let mut categories = entries
        .iter()
        .map(|emoji| emoji.category)
        .collect::<Vec<_>>();
    categories.dedup();
    let category_buttons = categories
        .iter()
        .enumerate()
        .map(|(index, category)| {
            let selected = if index == 0 { "true" } else { "false" };
            format!(
                "<button type=\"button\" role=\"tab\" aria-selected=\"{selected}\" data-emoji-category=\"{category}\">{}</button>",
                escape_html(category)
            )
        })
        .collect::<String>();
    let emoji_buttons = entries
        .iter()
        .map(|emoji| {
            format!(
                "<button type=\"button\" class=\"emoji-choice\" data-emoji-name=\"{}\" data-category=\"{}\" data-search=\"{}\" title=\":{}:\" aria-label=\"{}\">{}</button>",
                escape_html(&emoji.name),
                escape_html(emoji.category),
                escape_html(&format!("{} {}", emoji.name, emoji.label).to_lowercase()),
                escape_html(&emoji.name),
                escape_html(&emoji.label),
                emoji_value_html(&emoji.value, true),
            )
        })
        .collect::<String>();

    format!(
        "<dialog id=\"reaction-picker\" class=\"reaction-picker\" aria-labelledby=\"reaction-picker-title\"><header><h2 id=\"reaction-picker-title\">{}</h2><button type=\"button\" class=\"picker-close\" aria-label=\"{}\">×</button></header><label class=\"emoji-search-label\" for=\"emoji-search\">{}</label><input id=\"emoji-search\" class=\"emoji-search\" type=\"search\" autocomplete=\"off\" placeholder=\"{}\"><nav class=\"emoji-categories\" role=\"tablist\" aria-label=\"{}\">{category_buttons}</nav><div class=\"emoji-grid\" role=\"grid\" aria-label=\"{}\">{emoji_buttons}</div><p class=\"emoji-empty\" role=\"status\" hidden>{}</p></dialog>",
        escape_html(&gettext("Add reaction")),
        escape_html(&gettext("Close reaction picker")),
        escape_html(&gettext("Search emoji by name")),
        escape_html(&gettext("Search emoji")),
        escape_html(&gettext("Emoji categories")),
        escape_html(&gettext("Emoji")),
        escape_html(&gettext("No emoji found")),
    )
}

fn reaction_picker_script() -> &'static str {
    r##"(function () {
  const picker = document.getElementById("reaction-picker");
  if (!picker) return;
  const search = picker.querySelector("#emoji-search");
  const choices = Array.from(picker.querySelectorAll(".emoji-choice"));
  const tabs = Array.from(picker.querySelectorAll("[data-emoji-category]"));
  const empty = picker.querySelector(".emoji-empty");
  let activeCategory = "Smileys";
  let reactionTemplate = "";
  let opener = null;

  function filterChoices() {
    const query = search.value.trim().toLocaleLowerCase();
    let visible = 0;
    choices.forEach(function (choice) {
      const matchesQuery = !query || choice.dataset.search.includes(query) || choice.dataset.emojiName.includes(query);
      const matchesCategory = query || choice.dataset.category === activeCategory;
      choice.hidden = !(matchesQuery && matchesCategory);
      if (!choice.hidden) {
        const image = choice.querySelector("img[data-src]");
        if (image) {
          image.src = image.dataset.src;
          image.removeAttribute("data-src");
        }
        visible += 1;
      }
    });
    empty.hidden = visible !== 0;
  }

  document.addEventListener("click", function (event) {
    const menuAction = event.target.closest(".more-actions-menu a");
    if (menuAction) {
      const menu = menuAction.closest("details");
      if (menu) menu.open = false;
    }
    const trigger = event.target.closest("[data-open-reaction-picker]");
    if (!trigger) return;
    event.preventDefault();
    opener = trigger;
    reactionTemplate = trigger.dataset.reactionTemplate;
    search.value = "";
    filterChoices();
    picker.showModal();
    search.focus();
  });

  picker.querySelector(".picker-close").addEventListener("click", function () { picker.close(); });
  picker.addEventListener("close", function () { if (opener) opener.focus(); });
  search.addEventListener("input", filterChoices);
  tabs.forEach(function (tab) {
    tab.addEventListener("click", function () {
      activeCategory = tab.dataset.emojiCategory;
      tabs.forEach(function (item) { item.setAttribute("aria-selected", String(item === tab)); });
      search.value = "";
      filterChoices();
      const first = choices.find(function (choice) { return !choice.hidden; });
      if (first) first.focus();
    });
  });
  choices.forEach(function (choice) {
    choice.addEventListener("click", function () {
      const url = reactionTemplate.replace("__REACTION__", encodeURIComponent(choice.dataset.emojiName));
      picker.close();
      window.location.href = url;
    });
  });
})();"##
}

fn emoji_value_html(value: &EmojiValue, lazy: bool) -> String {
    match value {
        EmojiValue::Unicode(glyph) => escape_html(glyph),
        EmojiValue::CustomImage(url) if lazy => format!(
            "<img class=\"custom-emoji\" data-src=\"{}\" alt=\"\" aria-hidden=\"true\">",
            escape_html(url)
        ),
        EmojiValue::CustomImage(url) => format!(
            "<img class=\"custom-emoji\" src=\"{}\" alt=\"\" aria-hidden=\"true\" loading=\"lazy\">",
            escape_html(url)
        ),
    }
}

pub fn placeholder_document(title: &str, message: &str) -> String {
    html_document(
        title,
        &format!(
            "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<p class=\"placeholder\">{}</p></main>",
            document_heading(title),
            escape_html(message)
        ),
    )
}

#[cfg(test)]
pub fn conversation_document(
    channel_id: &str,
    messages: &[SlackMessage],
    context: &MessageHtmlContext,
) -> String {
    conversation_document_with_focus(channel_id, messages, context, None)
}

pub fn conversation_document_with_focus(
    channel_id: &str,
    messages: &[SlackMessage],
    context: &MessageHtmlContext,
    focus_message_ts: Option<&str>,
) -> String {
    if messages.is_empty() {
        return placeholder_document(&gettext("Messages"), &gettext("No messages"));
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

    let title = gettext("Messages");
    let focus_attribute = focus_message_ts
        .filter(|message_ts| !message_ts.is_empty())
        .map(|message_ts| format!(" data-focus-message-ts=\"{}\"", escape_html(message_ts)))
        .unwrap_or_default();
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\"{focus_attribute}>{}",
        document_heading(&title)
    );
    if context.thread_ts.is_none() {
        if let Some(url) = context.load_more_url.as_deref() {
            body.push_str(&load_more_action_html(url, &gettext("Load older messages")));
        }
    }
    body.push_str("<ol class=\"message-list\">");
    for group in groups {
        body.push_str("<li class=\"message-list-item\">");
        body.push_str(&message_group_article(Some(channel_id), &group, context));
        body.push_str("</li>");
    }
    body.push_str("</ol>");
    if context.thread_ts.is_some() {
        if let Some(url) = context.load_more_url.as_deref() {
            body.push_str(&load_more_action_html(url, &gettext("Load more replies")));
        }
    }
    if context.thread_ts.is_none() && context.read_marker_url.is_some() {
        body.push_str("<div id=\"conversation-read-sentinel\" aria-hidden=\"true\"></div>");
    }
    body.push_str("</main>");
    body.push_str(&reaction_picker_html(context));

    let mut scripts = Vec::new();
    if let Some(scroll_script) = timeline_scroll_script(channel_id, context.timeline_scroll) {
        scripts.push(scroll_script);
    }
    if !focus_attribute.is_empty() {
        scripts.push(message_focus_script().to_string());
    }
    if let Some(url) = context.read_marker_url.as_deref() {
        scripts.push(read_marker_script(url));
    }
    let script = (!scripts.is_empty()).then(|| scripts.join("\n"));
    html_document_with_script(&title, &body, script.as_deref())
}

pub fn saved_items_document(items: &[SavedItem], context: &MessageHtmlContext) -> String {
    let title = gettext("Saved items");
    let mut rendered = 0;
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ol class=\"message-list\">",
        document_heading(&title)
    );

    for item in items {
        if let (Some(channel_id), Some(message)) = (item.channel.as_deref(), item.message.as_ref())
        {
            body.push_str("<li class=\"message-list-item\">");
            body.push_str(&message_article(Some(channel_id), message, context));
            body.push_str("</li>");
            rendered += 1;
        }
    }

    body.push_str("</ol>");
    if rendered == 0 {
        body.push_str(&format!(
            "<p class=\"placeholder\">{}</p>",
            escape_html(&gettext("No saved items"))
        ));
    }
    body.push_str("</main>");
    if rendered > 0 {
        body.push_str(&reaction_picker_html(context));
    }

    html_document(&title, &body)
}

pub fn unreads_document(items: &[ActivityItem]) -> String {
    if items.is_empty() {
        return placeholder_document(&gettext("Unreads"), &gettext("No unread conversations"));
    }

    let title = gettext("Unreads");
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ul class=\"activity-list\">",
        document_heading(&title)
    );
    for item in items {
        body.push_str(&activity_item_html(item));
    }
    body.push_str("</ul></main>");

    html_document(&title, &body)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadInboxItem {
    pub channel_id: String,
    pub channel_title: String,
    pub root: SlackMessage,
}

pub fn threads_document(items: &[ThreadInboxItem], context: &MessageHtmlContext) -> String {
    if items.is_empty() {
        return placeholder_document(
            &gettext("Threads"),
            &gettext("Threads you open or participate in will appear here"),
        );
    }

    let title = gettext("Threads");
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ol class=\"message-list\">",
        document_heading(&title)
    );
    for item in items {
        let reply_count = item.root.reply_count.unwrap_or_default();
        let label = gettext("{channel} · {count} replies")
            .replace("{channel}", &item.channel_title)
            .replace("{count}", &reply_count.to_string());
        body.push_str(&format!(
            "<li class=\"message-list-item\"><a class=\"activity-row\" href=\"{}\">{}</a>{}</li>",
            escape_html(&thread_action_url(&item.channel_id, &item.root.ts)),
            escape_html(&label),
            message_article(Some(&item.channel_id), &item.root, context),
        ));
    }
    body.push_str("</ol></main>");
    body.push_str(&reaction_picker_html(context));
    html_document(&title, &body)
}

pub fn files_document(files: &[SlackFile]) -> String {
    if files.is_empty() {
        return placeholder_document(&gettext("Files"), &gettext("No files"));
    }

    let title = gettext("Files");
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ul class=\"file-list\">",
        document_heading(&title)
    );
    for file in files {
        body.push_str(&file_item_html(file));
    }
    body.push_str("</ul></main>");

    html_document(&title, &body)
}

pub fn search_results_document(results: &[SearchMatch], context: &MessageHtmlContext) -> String {
    if results.is_empty() {
        return placeholder_document(&gettext("Search results"), &gettext("No results"));
    }

    let title = gettext("Search results");
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ol class=\"message-list\">",
        document_heading(&title)
    );
    for result in results {
        body.push_str("<li class=\"message-list-item\">");
        body.push_str(&search_result_article(result, context));
        body.push_str("</li>");
    }
    body.push_str("</ol></main>");
    body.push_str(&reaction_picker_html(context));

    html_document(&title, &body)
}

fn document_direction(language: &str) -> &'static str {
    if let Some(script) = language.split('-').skip(1).find(|subtag| {
        subtag.len() == 4
            && subtag
                .chars()
                .all(|character| character.is_ascii_alphabetic())
    }) {
        return if ["Arab", "Hebr", "Nkoo", "Rohg", "Syrc", "Thaa", "Adlm"]
            .iter()
            .any(|rtl_script| script.eq_ignore_ascii_case(rtl_script))
        {
            "rtl"
        } else {
            "ltr"
        };
    }

    match language.split('-').next().unwrap_or_default() {
        "ar" | "arc" | "ckb" | "dv" | "fa" | "he" | "iw" | "nqo" | "ps" | "sd" | "syr" | "ug"
        | "ur" | "yi" => "rtl",
        _ => "ltr",
    }
}

fn html_document(title: &str, body: &str) -> String {
    html_document_with_script(title, body, None)
}

fn html_document_with_script(title: &str, body: &str, script: Option<&str>) -> String {
    html_document_with_language(title, body, script, &document_language())
}

fn html_document_with_language(
    title: &str,
    body: &str,
    script: Option<&str>,
    language: &str,
) -> String {
    let has_message_actions = body.contains("class=\"quick-actions\"");
    let scripts = [
        script.filter(|script| !script.trim().is_empty()),
        has_message_actions.then_some(reaction_picker_script()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");
    let script_tag = if scripts.is_empty() {
        String::new()
    } else {
        format!("\n<script>\n{scripts}\n</script>")
    };
    format!(
        r#"<!doctype html>
<html lang="{}" dir="{}">
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
  --accent: #0061c9;
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
  min-block-size: 100%;
  margin: 0;
  background: var(--page);
  color: var(--text);
  font: 14px/1.45 Cantarell, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}}

body {{
  overflow-wrap: anywhere;
}}

:where(a, button, input, summary, [tabindex]):focus-visible {{
  outline: 3px solid var(--accent);
  outline-offset: 2px;
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
  inline-size: 100%;
  max-inline-size: 880px;
  margin-block: 0;
  margin-inline: auto;
  padding-block: 4px 20px;
  padding-inline: 12px;
}}

.visually-hidden {{
  position: absolute;
  inline-size: 1px;
  block-size: 1px;
  padding: 0;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
  border: 0;
}}

.message-list,
.activity-list,
.file-list {{
  margin: 0;
  padding: 0;
  list-style: none;
}}

.message-list-item {{
  display: block;
}}

.message {{
  position: relative;
  display: grid;
  gap: 6px;
  padding-block: 10px;
  padding-inline: 0;
  border-block-end: 1px solid var(--line);
}}

.message:focus-visible,
.message-part:focus-visible {{
  border-radius: 6px;
  outline: 3px solid var(--accent);
  outline-offset: -3px;
}}

.message-stack {{
  display: grid;
  gap: 8px;
}}

.message-part {{
  position: relative;
  display: grid;
  gap: 6px;
}}

.message-part + .message-part {{
  padding-block-start: 4px;
}}

.message-header {{
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-inline-size: 0;
}}

.author {{
  min-inline-size: 0;
  font-weight: 700;
}}

.metadata {{
  color: var(--muted);
  font-size: 12px;
}}

.body {{
  min-inline-size: 0;
}}

.body p {{
  margin-block: 0;
  margin-inline: 0;
}}

.context-block,
.empty-message,
.attachment,
.image-alt {{
  color: var(--muted);
}}

.divider {{
  block-size: 1px;
  border: 0;
  background: var(--line);
  margin-block: 4px;
  margin-inline: 0;
}}

code {{
  padding-block: 1px;
  padding-inline: 4px;
  border-radius: 4px;
  background: var(--code);
  font-family: ui-monospace, "Cascadia Mono", "SF Mono", Menlo, Consolas, monospace;
  font-size: 13px;
}}

pre {{
  margin-block: 2px;
  margin-inline: 0;
  padding-block: 10px;
  padding-inline: 10px;
  overflow-x: auto;
  border-radius: 8px;
  background: var(--code);
}}

pre code {{
  padding-block: 0;
  padding-inline: 0;
  border-radius: 0;
  background: transparent;
  font-size: 13px;
}}

.mention {{
  display: inline-block;
  padding-block: 0;
  padding-inline: 4px;
  border-radius: 4px;
  background: var(--accent-soft);
  font-weight: 700;
}}

.emoji {{
  font-family: "Noto Color Emoji", "Apple Color Emoji", "Segoe UI Emoji", sans-serif;
  line-height: 1;
}}

.custom-emoji {{
  display: inline-block;
  inline-size: 1.25em;
  block-size: 1.25em;
  object-fit: contain;
  vertical-align: -0.25em;
}}

.attachments,
.reactions,
.block-actions {{
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
}}

.attachment,
.block-action {{
  padding-block: 3px;
  padding-inline: 7px;
  border-radius: 6px;
  background: var(--soft);
}}

.image-attachment {{
  display: grid;
  gap: 6px;
  max-inline-size: 520px;
  margin-block: 2px;
  margin-inline: 0;
}}

.image-attachment img {{
  display: block;
  inline-size: auto;
  max-inline-size: 100%;
  max-block-size: 420px;
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
  min-block-size: 72px;
  padding-block: 10px;
  padding-inline: 10px;
  border-radius: 8px;
  background: var(--soft);
  color: var(--muted);
}}

.activity-list {{
  display: grid;
  gap: 0;
  border-block-start: 1px solid var(--line);
}}

.activity-row {{
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 4px 12px;
  padding-block: 11px;
  padding-inline: 0;
  border-block-end: 1px solid var(--line);
  color: var(--text);
}}

.activity-row:hover {{
  text-decoration: none;
}}

.activity-title {{
  min-inline-size: 0;
  font-weight: 700;
}}

.activity-meta {{
  color: var(--muted);
  font-size: 12px;
}}

.activity-badge {{
  align-self: center;
  grid-row: 1 / span 2;
  grid-column: 2;
  min-inline-size: 24px;
  padding-block: 2px;
  padding-inline: 8px;
  border-radius: 999px;
  background: var(--accent-soft);
  color: var(--text);
  font-size: 12px;
  text-align: center;
}}

.file-list {{
  display: grid;
  gap: 0;
  border-block-start: 1px solid var(--line);
}}

.file-row {{
  display: grid;
  gap: 4px;
  padding-block: 11px;
  padding-inline: 0;
  border-block-end: 1px solid var(--line);
  color: var(--text);
}}

.file-row:hover {{
  text-decoration: none;
}}

.file-title {{
  min-inline-size: 0;
  font-weight: 700;
}}

.file-meta {{
  color: var(--muted);
  font-size: 12px;
}}

.reaction {{
  padding-block: 2px;
  padding-inline: 7px;
  border-radius: 999px;
  background: var(--soft);
  color: var(--muted);
  font-size: 12px;
}}

.reaction.thread-reaction {{
  color: var(--accent);
  font-weight: 600;
}}

.reaction.thread-reaction:hover {{
  background: var(--accent-soft);
  text-decoration: none;
}}

.reaction.is-active {{
  background: var(--success-soft);
  color: var(--text);
  font-weight: 700;
}}

.quick-actions {{
  position: static;
  display: flex;
  flex-wrap: wrap;
  justify-self: end;
  align-items: center;
  gap: 0;
  max-inline-size: 100%;
  overflow: visible;
  border: 1px solid var(--line);
  padding-block: 2px;
  padding-inline: 2px;
  border-radius: 10px;
  background: var(--page);
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.12);
  opacity: 1;
  pointer-events: auto;
  transition: opacity 120ms ease;
}}

.action-button {{
  display: inline-flex;
  justify-content: center;
  align-items: center;
  min-inline-size: 36px;
  min-block-size: 36px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: var(--text);
  font: inherit;
  line-height: 1;
  cursor: pointer;
  text-decoration: none;
}}

.action-button:hover {{
  background: var(--soft);
  text-decoration: none;
}}

.action-button.is-active {{
  background: var(--success-soft);
}}

.action-divider {{
  inline-size: 1px;
  block-size: 24px;
  margin-inline: 2px;
  background: var(--line);
}}

.more-actions {{
  position: relative;
}}

.more-actions > summary {{
  list-style: none;
}}

.more-actions > summary::-webkit-details-marker {{
  display: none;
}}

.more-actions-menu {{
  position: absolute;
  z-index: 4;
  inset-block-start: calc(100% + 6px);
  inset-inline-end: 0;
  display: grid;
  min-inline-size: 190px;
  padding-block: 6px;
  padding-inline: 6px;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: var(--page);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
}}

.more-action {{
  display: block;
  padding-block: 8px;
  padding-inline: 10px;
  border-radius: 6px;
  color: var(--text);
  white-space: nowrap;
}}

.more-action:hover {{
  background: var(--soft);
  text-decoration: none;
}}

.reaction-picker {{
  box-sizing: border-box;
  inline-size: min(520px, calc(100vw - 24px));
  max-block-size: min(620px, calc(100vh - 24px));
  padding: 0;
  overflow: hidden;
  border: 1px solid var(--line);
  border-radius: 14px;
  background: var(--page);
  color: var(--text);
  box-shadow: 0 16px 48px rgba(0, 0, 0, 0.28);
}}

.reaction-picker::backdrop {{
  background: rgba(0, 0, 0, 0.28);
}}

.reaction-picker > header {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding-block: 14px 8px;
  padding-inline: 16px;
}}

.reaction-picker h2 {{
  margin: 0;
  font-size: 18px;
}}

.picker-close,
.emoji-categories button,
.emoji-choice {{
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
}}

.emoji-choice .custom-emoji {{
  inline-size: 30px;
  block-size: 30px;
  vertical-align: middle;
}}

.picker-close {{
  display: grid;
  place-items: center;
  inline-size: 34px;
  block-size: 34px;
  border-radius: 50%;
  font-size: 24px;
}}

.picker-close:hover,
.emoji-choice:hover {{
  background: var(--soft);
}}

.emoji-search-label {{
  position: absolute;
  inline-size: 1px;
  block-size: 1px;
  overflow: hidden;
  clip-path: inset(50%);
}}

.emoji-search {{
  box-sizing: border-box;
  inline-size: calc(100% - 32px);
  min-block-size: 40px;
  margin-block: 0 10px;
  margin-inline: 16px;
  padding-block: 8px;
  padding-inline: 12px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--soft);
  color: var(--text);
  font: inherit;
}}

.emoji-categories {{
  display: flex;
  gap: 2px;
  padding-inline: 12px;
  overflow-x: auto;
  border-block-end: 1px solid var(--line);
}}

.emoji-categories button {{
  min-block-size: 36px;
  padding-inline: 9px;
  border-block-end: 2px solid transparent;
  color: var(--muted);
  white-space: nowrap;
}}

.emoji-categories button[aria-selected="true"] {{
  border-block-end-color: var(--accent);
  color: var(--text);
  font-weight: 700;
}}

.emoji-grid {{
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(42px, 1fr));
  gap: 4px;
  max-block-size: 360px;
  padding-block: 12px;
  padding-inline: 12px;
  overflow-y: auto;
  overscroll-behavior: contain;
  scrollbar-gutter: stable;
}}

.emoji-choice {{
  display: grid;
  place-items: center;
  min-block-size: 42px;
  border-radius: 8px;
  font-family: "Noto Color Emoji", "Apple Color Emoji", "Segoe UI Emoji", sans-serif;
  font-size: 24px;
}}

.emoji-choice[hidden] {{
  display: none;
}}

.emoji-empty {{
  margin: 0;
  padding-block: 24px;
  padding-inline: 16px;
  color: var(--muted);
  text-align: center;
}}

.external-actions {{
  display: flex;
  gap: 8px;
}}

.external-action {{
  color: var(--accent);
  font-size: 13px;
}}

.timeline-action {{
  display: flex;
  justify-content: center;
  padding-block: 10px;
  padding-inline: 0;
}}

.timeline-action a {{
  display: inline-flex;
  align-items: center;
  min-block-size: 30px;
  padding-block: 0;
  padding-inline: 12px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--soft);
  color: var(--text);
  font-size: 13px;
}}

.timeline-action a:hover {{
  text-decoration: none;
  background: var(--accent-soft);
}}

.image-link {{
  color: inherit;
  text-decoration: none;
}}

.image-link:hover {{
  text-decoration: none;
}}

.placeholder {{
  padding-block: 14px;
  padding-inline: 0;
  color: var(--muted);
}}

@media (hover: hover) and (pointer: fine) {{
  .message,
  .message-part {{
    grid-template-columns: minmax(0, 1fr) auto;
  }}

  .message > :not(.quick-actions),
  .message-part > :not(.quick-actions) {{
    grid-column: 1;
  }}

  .quick-actions {{
    grid-row: 1;
    grid-column: 2;
    align-self: start;
    opacity: 0;
    pointer-events: none;
  }}

  .message:hover > .quick-actions,
  .message:focus-within > .quick-actions,
  .message-part:hover > .quick-actions,
  .message-part:focus-within > .quick-actions,
  .quick-actions:focus-within {{
    opacity: 1;
    pointer-events: auto;
  }}
}}

@media (prefers-reduced-motion: reduce) {{
  *,
  *::before,
  *::after {{
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    scroll-behavior: auto !important;
    transition-duration: 0.01ms !important;
  }}
}}
</style>
</head>
<body>
{}
{}
</body>
</html>"#,
        escape_html(language),
        document_direction(language),
        escape_html(title),
        body,
        script_tag
    )
}

fn timeline_scroll_script(channel_id: &str, behavior: TimelineScrollBehavior) -> Option<String> {
    if behavior == TimelineScrollBehavior::Preserve {
        return None;
    }

    let sticky_key = serde_json::to_string(&format!("conduit:timeline-at-bottom:{channel_id}"))
        .unwrap_or_else(|_| "\"conduit:timeline-at-bottom:\"".to_string());
    let anchor_key = serde_json::to_string(&format!("conduit:timeline-anchor:{channel_id}"))
        .unwrap_or_else(|_| "\"conduit:timeline-anchor:\"".to_string());
    let mode = serde_json::to_string(behavior.js_mode()).unwrap_or_else(|_| "\"preserve\"".into());
    Some(format!(
        r#"(function () {{
  const mode = {mode};
  const stickyKey = {sticky_key};
  const anchorKey = {anchor_key};
  const threshold = 48;

  function root() {{
    return document.scrollingElement || document.documentElement;
  }}

  function atBottom() {{
    const scrollRoot = root();
    return scrollRoot.scrollHeight - scrollRoot.scrollTop - scrollRoot.clientHeight <= threshold;
  }}

  function rememberPosition() {{
    try {{
      sessionStorage.setItem(stickyKey, atBottom() ? "true" : "false");
    }} catch (_) {{
    }}
  }}

  function wasAtBottom() {{
    if (mode === "bottom") {{
      return true;
    }}
    try {{
      return sessionStorage.getItem(stickyKey) !== "false";
    }} catch (_) {{
      return true;
    }}
  }}

  function messageAnchors() {{
    return Array.from(document.querySelectorAll("[data-message-ts]"));
  }}

  function visibleAnchor() {{
    const viewportHeight = window.innerHeight || root().clientHeight;
    return messageAnchors().find(function (element) {{
      const rect = element.getBoundingClientRect();
      return rect.bottom >= 0 && rect.top <= viewportHeight;
    }});
  }}

  function rememberAnchor() {{
    const scrollRoot = root();
    const anchor = visibleAnchor();
    const payload = {{
      scrollTop: scrollRoot.scrollTop,
      scrollHeight: scrollRoot.scrollHeight
    }};
    if (anchor) {{
      payload.ts = anchor.dataset.messageTs;
      payload.top = anchor.getBoundingClientRect().top;
    }}
    try {{
      sessionStorage.setItem(anchorKey, JSON.stringify(payload));
    }} catch (_) {{
    }}
  }}

  function restorePrependAnchor() {{
    let payload = null;
    try {{
      payload = JSON.parse(sessionStorage.getItem(anchorKey) || "null");
    }} catch (_) {{
      payload = null;
    }}
    if (!payload) {{
      return;
    }}

    const scrollRoot = root();
    const anchor = payload.ts
      ? messageAnchors().find(function (element) {{
          return element.dataset.messageTs === payload.ts;
        }})
      : null;
    if (anchor && typeof payload.top === "number") {{
      scrollRoot.scrollTop += anchor.getBoundingClientRect().top - payload.top;
    }} else if (
      typeof payload.scrollTop === "number" &&
      typeof payload.scrollHeight === "number"
    ) {{
      scrollRoot.scrollTop = payload.scrollTop + scrollRoot.scrollHeight - payload.scrollHeight;
    }}
    rememberPosition();
  }}

  function scrollToBottom() {{
    const scrollRoot = root();
    scrollRoot.scrollTop = scrollRoot.scrollHeight;
    rememberPosition();
  }}

  const shouldStick = wasAtBottom();
  function applyScroll() {{
    if (shouldStick) {{
      scrollToBottom();
    }} else {{
      rememberPosition();
    }}
  }}

  document.addEventListener("click", function (event) {{
    const target = event.target && event.target.closest
      ? event.target.closest("a[href^='conduit://load-older']")
      : null;
    if (target) {{
      rememberAnchor();
    }}
  }}, true);

  if (mode === "preserve-prepend") {{
    window.addEventListener("scroll", rememberPosition, {{ passive: true }});
    window.addEventListener("load", restorePrependAnchor, {{ once: true }});
    requestAnimationFrame(restorePrependAnchor);
    requestAnimationFrame(function () {{
      requestAnimationFrame(restorePrependAnchor);
    }});
    return;
  }}

  window.addEventListener("scroll", rememberPosition, {{ passive: true }});
  window.addEventListener("load", applyScroll, {{ once: true }});
  requestAnimationFrame(applyScroll);
  requestAnimationFrame(function () {{
    requestAnimationFrame(applyScroll);
  }});
}})();"#
    ))
}

fn message_focus_script() -> &'static str {
    r#"(function () {
  function focusTarget() {
    const timeline = document.querySelector("[data-focus-message-ts]");
    if (!timeline) {
      return;
    }
    const targetTs = timeline.dataset.focusMessageTs;
    const target = Array.from(document.querySelectorAll("[data-message-ts]")).find(
      function (element) { return element.dataset.messageTs === targetTs; }
    );
    if (!target) {
      return;
    }
    target.scrollIntoView({ block: "center", inline: "nearest" });
    try {
      target.focus({ preventScroll: true });
    } catch (_) {
      target.focus();
    }
  }

  window.addEventListener("load", focusTarget, { once: true });
  requestAnimationFrame(focusTarget);
  requestAnimationFrame(function () { requestAnimationFrame(focusTarget); });
})();"#
}

fn read_marker_script(url: &str) -> String {
    let url = serde_json::to_string(url).expect("read marker URL should serialize");
    format!(
        r#"(function () {{
  const sentinel = document.getElementById("conversation-read-sentinel");
  if (!sentinel || !("IntersectionObserver" in window)) return;
  let sent = false;
  const observer = new IntersectionObserver(function (entries) {{
    if (sent || !entries.some(function (entry) {{ return entry.isIntersecting; }})) return;
    sent = true;
    observer.disconnect();
    window.location.href = {url};
  }}, {{ threshold: 1.0 }});
  observer.observe(sentinel);
}})();"#
    )
}

fn message_article(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let author = author_label(message, context);
    let mut article = format!(
        "<article class=\"message\"{}><header class=\"message-header\"><span class=\"author\" dir=\"auto\">{}</span>{}</header><div class=\"body\" dir=\"auto\">{}</div>",
        message_target_attributes(Some(&message.ts)),
        escape_html(&author),
        metadata_html(message),
        message_body_html(message, context)
    );

    article.push_str(&attachments_html(channel_id, message, context));
    article.push_str(&message_responses_html(channel_id, message, context));
    article.push_str(&message_actions_html(channel_id, message, context));
    article.push_str("</article>");
    article
}

fn load_more_action_html(url: &str, label: &str) -> String {
    format!(
        "<nav class=\"timeline-action\"><a href=\"{}\">{}</a></nav>",
        escape_html(url),
        escape_html(label)
    )
}

fn activity_item_html(item: &ActivityItem) -> String {
    format!(
        concat!(
            "<li><a class=\"activity-row\" href=\"{}\">",
            "<span class=\"activity-title\" dir=\"auto\">{}</span>",
            "<span class=\"activity-badge\">{}</span>",
            "<span class=\"activity-meta\">{}</span>",
            "</a></li>"
        ),
        escape_html(&unreads_open_action_url(&item.channel_id)),
        escape_html(&item.title),
        escape_html(&item.unread_label()),
        escape_html(&item.kind.label())
    )
}

fn file_item_html(file: &SlackFile) -> String {
    let title = escape_html(file.display_title());
    let detail = file.detail_label();
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!("<span class=\"file-meta\">{}</span>", escape_html(&detail))
    };
    let content = format!("<span class=\"file-title\" dir=\"auto\">{title}</span>{detail}");

    if let Some(url) = file.link_url().filter(|url| is_http_url(url)) {
        format!(
            "<li><a class=\"file-row\" href=\"{}\" rel=\"noreferrer noopener\">{content}</a></li>",
            escape_html(url)
        )
    } else {
        format!("<li><div class=\"file-row\">{content}</div></li>")
    }
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
        "<article class=\"message message-group\"><header class=\"message-header\"><span class=\"author\" dir=\"auto\">{}</span>{}</header><div class=\"message-stack\">",
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
        "<div class=\"message-part\"{}><div class=\"body\" dir=\"auto\">{}</div>",
        message_target_attributes(Some(&message.ts)),
        message_body_html(message, context)
    );

    part.push_str(&attachments_html(channel_id, message, context));
    part.push_str(&message_responses_html(channel_id, message, context));
    part.push_str(&message_actions_html(channel_id, message, context));
    part.push_str("</div>");
    part
}

fn message_target_attributes(ts: Option<&str>) -> String {
    let Some(ts) = ts.filter(|ts| !ts.is_empty()) else {
        return String::new();
    };

    let mut id = String::from("message-");
    for byte in ts.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            id.push(char::from(byte));
        } else {
            id.push_str(&format!("-{byte:02x}"));
        }
    }

    format!(
        " id=\"{}\" tabindex=\"-1\" data-message-ts=\"{}\"",
        escape_html(&id),
        escape_html(ts)
    )
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
        .map(ToString::to_string)
        .unwrap_or_else(|| gettext("Unknown"));
    let text = result.text.as_deref().unwrap_or_default();

    let timestamp = result.ts.as_deref().map(timestamp_html).unwrap_or_default();
    let mut article = format!(
        "<article class=\"message\"{}><header class=\"message-header\"><span class=\"author\" dir=\"auto\">{}</span><span class=\"metadata\">{}</span>{}</header><div class=\"body\" dir=\"auto\"><p>{}</p></div>",
        message_target_attributes(result.ts.as_deref()),
        escape_html(&author),
        escape_html(&channel),
        timestamp,
        mrkdwn_to_html(text, context)
    );

    let mut actions = String::new();
    if let Some(location) = result.message_location() {
        actions.push_str(&format!(
            "<a class=\"external-action\" href=\"{}\">{}</a>",
            escape_html(&message_context_action_url(&location)),
            escape_html(&gettext("Open in Conduit"))
        ));
    }
    if let Some(permalink) = result.permalink.as_deref().filter(|url| is_http_url(url)) {
        actions.push_str(&format!(
            "<a class=\"external-action\" href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
            escape_html(permalink),
            escape_html(&gettext("Open in Slack"))
        ));
    }
    if !actions.is_empty() {
        article.push_str(&format!(
            "<nav class=\"external-actions\" aria-label=\"{}\">{actions}</nav>",
            escape_html(&gettext("Message actions")),
        ));
    }

    article.push_str("</article>");
    article
}

fn metadata_html(message: &SlackMessage) -> String {
    let mut metadata = timestamp_html(&message.ts);

    if message.edited.is_some() {
        metadata.push_str(&format!(
            "<span class=\"metadata\">{}</span>",
            escape_html(&gettext("edited"))
        ));
    }

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
    let Some((machine, full, short)) = localized_timestamp_parts(ts) else {
        return String::new();
    };

    format!(
        "<time class=\"metadata\" datetime=\"{}\" title=\"{}\">{}</time>",
        escape_html(&machine),
        escape_html(&full),
        escape_html(&short)
    )
}

fn localized_timestamp_parts(ts: &str) -> Option<(String, String, String)> {
    let datetime = slack_ts_datetime(ts)?;
    let machine = datetime.format_iso8601().ok()?.to_string();
    let full = datetime.format("%c %Z").ok()?.to_string();
    let short = datetime.format("%X").ok()?.to_string();
    Some((machine, full, short))
}

fn slack_ts_datetime(ts: &str) -> Option<gtk::glib::DateTime> {
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

fn author_label(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    message
        .user
        .as_ref()
        .and_then(|user_id| context.user_names.get(user_id))
        .cloned()
        .unwrap_or_else(|| message.author_label())
}

fn message_body_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    if message.subtype.as_deref() == Some("message_deleted") {
        return format!(
            "<p class=\"empty-message\">{}</p>",
            escape_html(&gettext("Message deleted"))
        );
    }

    if let Some(blocks) = message.blocks.as_ref() {
        let rendered = blocks_html(blocks, context);
        if !rendered.is_empty() {
            return rendered;
        }
    }

    let text = message.body_text();
    if text.trim().is_empty() {
        format!(
            "<p class=\"empty-message\">{}</p>",
            escape_html(&gettext("No message text"))
        )
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
                    .map(ToString::to_string)
                    .unwrap_or_else(|| gettext("Image"));
                if let Some(url) = block
                    .get("image_url")
                    .and_then(|url| url.as_str())
                    .filter(|url| is_http_url(url))
                {
                    rendered.push_str(&image_figure_html(
                        url,
                        Some(url),
                        &alt,
                        Some(&gettext("Slack image")),
                        context,
                    ));
                } else {
                    rendered.push_str(&format!(
                        "<p class=\"image-alt\">{}</p>",
                        escape_html(
                            &gettext("Image: {description}").replace("{description}", &alt)
                        )
                    ));
                }
            }
            "actions" => {
                let labels = block
                    .get("elements")
                    .and_then(|elements| elements.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|element| block_action_html(element, context))
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

fn block_action_html(element: &serde_json::Value, context: &MessageHtmlContext) -> Option<String> {
    let label = block_text(element)?;
    let label = mrkdwn_to_html(&label, context);

    if let Some(url) = element
        .get("url")
        .and_then(|url| url.as_str())
        .filter(|url| is_http_url(url))
    {
        Some(format!(
            "<a class=\"block-action\" href=\"{}\" rel=\"noreferrer noopener\">{label}</a>",
            escape_html(url)
        ))
    } else {
        Some(format!("<span class=\"block-action\">{label}</span>"))
    }
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

fn attachments_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let Some(files) = message.files.as_ref().filter(|files| !files.is_empty()) else {
        return String::new();
    };

    let attachments = files
        .iter()
        .map(|file| {
            let label = file.display_title();
            if let (Some(channel_id), Some(kind), Some(url)) =
                (channel_id, file.supported_media_kind(), file.media_url())
            {
                let viewer_url = media_action_url(channel_id, &message.ts, url, label, kind);
                if kind == "image" {
                    let preview_url = file.preview_url().unwrap_or(url);
                    return image_figure_html(
                        preview_url,
                        Some(&viewer_url),
                        label,
                        Some(label),
                        context,
                    );
                }
                return attachment_chip_html(label, Some(&viewer_url));
            }

            attachment_chip_html(
                label,
                file.permalink.as_deref().or(file.url_private.as_deref()),
            )
        })
        .collect::<String>();

    format!("<div class=\"attachments\">{attachments}</div>")
}

pub fn media_action_url(channel_id: &str, ts: &str, url: &str, name: &str, kind: &str) -> String {
    format!(
        "conduit://media?channel={}&ts={}&url={}&name={}&kind={}",
        encode_query(channel_id),
        encode_query(ts),
        encode_query(url),
        encode_query(name),
        encode_query(kind),
    )
}

fn attachment_chip_html(label: &str, link: Option<&str>) -> String {
    let label = escape_html(&gettext("Attachment: {name}").replace("{name}", label));
    if let Some(link) =
        link.filter(|link| is_http_url(link) || link.starts_with("conduit://media?"))
    {
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
        format!(
            "<div class=\"image-placeholder\">{}</div>",
            escape_html(&gettext("Image preview unavailable"))
        )
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
        format!(
            "<div class=\"image-placeholder\">{}</div>",
            escape_html(&gettext("Loading image preview"))
        )
    };

    let caption = caption
        .filter(|caption| !caption.trim().is_empty())
        .map(|caption| {
            format!(
                "<figcaption class=\"image-caption\" dir=\"auto\">{}</figcaption>",
                escape_html(caption)
            )
        })
        .unwrap_or_default();

    let figure = format!("<figure class=\"image-attachment\">{image}{caption}</figure>");
    if let Some(link) =
        link.filter(|link| is_http_url(link) || link.starts_with("conduit://media?"))
    {
        format!(
            "<a class=\"image-link\" href=\"{}\" rel=\"noreferrer noopener\">{figure}</a>",
            escape_html(link)
        )
    } else {
        figure
    }
}

fn message_responses_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let mut responses = reactions_html(message, context);
    responses.push_str(&thread_response_html(channel_id, message, context));

    if responses.is_empty() {
        String::new()
    } else {
        format!("<div class=\"reactions\">{responses}</div>")
    }
}

fn reactions_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    message
        .reactions
        .as_ref()
        .into_iter()
        .flatten()
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
            let participants = reaction
                .users
                .as_ref()
                .into_iter()
                .flatten()
                .map(|user_id| {
                    context
                        .user_names
                        .get(user_id)
                        .cloned()
                        .unwrap_or_else(|| user_id.to_string())
                })
                .collect::<Vec<_>>();
            let tooltip = if participants.is_empty() {
                gettext("No participant details available")
            } else {
                gettext("Reacted: {names}").replace("{names}", &participants.join(", "))
            };
            Some(format!(
                "<span class=\"reaction{}\" tabindex=\"0\" title=\"{}\" aria-label=\"{}\">{} {}</span>",
                active_class,
                escape_html(&tooltip),
                escape_html(&tooltip),
                reaction_label(name, context),
                count
            ))
        })
        .collect::<String>()
}

fn thread_response_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    if context.thread_ts.is_some() || !message.has_thread() || message.ts.is_empty() {
        return String::new();
    }

    let Some(channel_id) = channel_id else {
        return String::new();
    };

    let title = message
        .reply_count
        .filter(|count| *count > 0)
        .map(|count| gettext("View thread ({count})").replace("{count}", &count.to_string()))
        .unwrap_or_else(|| gettext("View thread"));

    let label = message
        .reply_count
        .filter(|count| *count > 0)
        .map(|count| gettext("thread ({count})").replace("{count}", &count.to_string()))
        .unwrap_or_else(|| gettext("thread"));

    format!(
        "<a class=\"reaction thread-reaction\" href=\"{}\" title=\"{}\" aria-label=\"{}\">{}</a>",
        escape_html(&thread_action_url(channel_id, &message.ts)),
        escape_html(&title),
        escape_html(&title),
        escape_html(&label)
    )
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

    let thread_ts = action_thread_ts(message, context);
    let mut actions = String::new();
    for emoji in recent_reactions(context) {
        let reacted = message.user_reacted(&emoji.name, context.current_user_id.as_deref());
        actions.push_str(&action_button_content_html(
            &reaction_action_url(channel_id, message, &emoji.name, !reacted, thread_ts),
            &emoji_value_html(&emoji.value, false),
            &gettext("React with {emoji}").replace("{emoji}", &emoji.label),
            reacted,
        ));
    }
    let reaction_template =
        reaction_action_url(channel_id, message, "__REACTION__", true, thread_ts);
    actions.push_str(&format!(
        "<button type=\"button\" class=\"action-button\" data-open-reaction-picker data-reaction-template=\"{}\" title=\"{}\" aria-label=\"{}\">☺<span aria-hidden=\"true\">+</span></button>",
        escape_html(&reaction_template),
        escape_html(&gettext("Add reaction")),
        escape_html(&gettext("Add reaction")),
    ));
    actions.push_str("<span class=\"action-divider\" aria-hidden=\"true\"></span>");

    if context.thread_ts.is_none() {
        let title = message
            .reply_count
            .filter(|count| *count > 0)
            .map(|count| gettext("View thread ({count})").replace("{count}", &count.to_string()))
            .unwrap_or_else(|| gettext("Reply in thread"));
        actions.push_str(&action_button_html(
            &thread_action_url(channel_id, &message.ts),
            "💬",
            &title,
            false,
        ));
    }

    actions.push_str(&action_button_html(
        &forward_action_url(channel_id, message),
        "↗",
        &gettext("Forward message"),
        false,
    ));

    let starred = message.is_starred.unwrap_or(false);
    let save_title = if starred {
        gettext("Remove from saved items")
    } else {
        gettext("Save for later")
    };
    actions.push_str(&format!(
        "<details class=\"more-actions\"><summary class=\"action-button\" title=\"{}\" aria-label=\"{}\">⋯</summary><div class=\"more-actions-menu\" role=\"menu\"><a class=\"more-action{}\" role=\"menuitem\" href=\"{}\">{}</a><a class=\"more-action\" role=\"menuitem\" href=\"{}\">{}</a><a class=\"more-action\" role=\"menuitem\" href=\"{}\">{}</a></div></details>",
        escape_html(&gettext("More actions")),
        escape_html(&gettext("More actions")),
        if starred { " is-active" } else { "" },
        escape_html(&save_action_url(channel_id, message, !starred, thread_ts)),
        escape_html(&save_title),
        escape_html(&copy_link_action_url(channel_id, message)),
        escape_html(&gettext("Copy link")),
        escape_html(&copy_message_action_url(channel_id, message)),
        escape_html(&gettext("Copy message")),
    ));

    format!(
        "<nav class=\"quick-actions\" aria-label=\"{}\">{actions}</nav>",
        escape_html(&gettext("Message actions"))
    )
}

pub fn forward_action_url(channel_id: &str, message: &SlackMessage) -> String {
    format!(
        "conduit://forward?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(&message.ts)
    )
}

pub fn thread_action_url(channel_id: &str, ts: &str) -> String {
    format!(
        "conduit://thread?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(ts)
    )
}

pub fn reaction_action_url(
    channel_id: &str,
    message: &SlackMessage,
    name: &str,
    add: bool,
    thread_ts: Option<&str>,
) -> String {
    let mut url = format!(
        "conduit://reaction?channel={}&ts={}&name={}&add={}",
        encode_query(channel_id),
        encode_query(&message.ts),
        encode_query(name),
        add
    );

    append_thread_ts_query(&mut url, thread_ts);

    url
}

pub fn save_action_url(
    channel_id: &str,
    message: &SlackMessage,
    add: bool,
    thread_ts: Option<&str>,
) -> String {
    let mut url = format!(
        "conduit://save?channel={}&ts={}&add={}",
        encode_query(channel_id),
        encode_query(&message.ts),
        add
    );

    append_thread_ts_query(&mut url, thread_ts);
    url
}

pub fn copy_message_action_url(channel_id: &str, message: &SlackMessage) -> String {
    format!(
        "conduit://copy-message?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(&message.ts)
    )
}

pub fn copy_link_action_url(channel_id: &str, message: &SlackMessage) -> String {
    format!(
        "conduit://copy-link?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(&message.ts)
    )
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

pub fn unreads_open_action_url(channel_id: &str) -> String {
    format!(
        "conduit://unreads-open?channel={}",
        encode_query(channel_id)
    )
}

pub fn mark_read_action_url(channel_id: &str, ts: &str) -> String {
    format!(
        "conduit://mark-read?channel={}&ts={}",
        encode_query(channel_id),
        encode_query(ts)
    )
}

pub fn message_context_action_url(location: &SearchMessageLocation) -> String {
    let mut url = format!(
        "conduit://message?channel={}&ts={}",
        encode_query(location.channel_id()),
        encode_query(location.message_ts())
    );
    append_thread_ts_query(&mut url, location.thread_ts());
    url
}

fn append_thread_ts_query(url: &mut String, thread_ts: Option<&str>) {
    if let Some(thread_ts) = thread_ts.filter(|ts| !ts.is_empty()) {
        url.push_str("&thread_ts=");
        url.push_str(&encode_query(thread_ts));
    }
}

fn action_thread_ts<'a>(
    message: &'a SlackMessage,
    context: &'a MessageHtmlContext,
) -> Option<&'a str> {
    context.thread_ts.as_deref().or_else(|| {
        message
            .thread_ts
            .as_deref()
            .filter(|thread_ts| !thread_ts.is_empty() && *thread_ts != message.ts.as_str())
    })
}

fn recent_reactions(context: &MessageHtmlContext) -> Vec<EmojiEntry> {
    let requested = context.recent_reactions.iter().map(String::as_str).chain([
        "smile",
        "thumbsup",
        "white_check_mark",
    ]);
    let mut seen = HashSet::new();
    requested
        .filter(|name| seen.insert(*name))
        .filter_map(|name| emoji_entry(name, context))
        .take(3)
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

fn action_button_html(href: &str, label: &str, title: &str, active: bool) -> String {
    action_button_content_html(href, &escape_html(label), title, active)
}

fn action_button_content_html(href: &str, content: &str, title: &str, active: bool) -> String {
    let active_class = if active { " is-active" } else { "" };
    format!(
        "<a class=\"action-button{}\" href=\"{}\" title=\"{}\" aria-label=\"{}\">{}</a>",
        active_class,
        escape_html(href),
        escape_html(title),
        escape_html(title),
        content
    )
}

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn reaction_label(name: &str, context: &MessageHtmlContext) -> String {
    EmojiCatalog::new(&context.custom_emojis)
        .resolve(name)
        .map(|value| emoji_value_html(&value, false))
        .unwrap_or_else(|| escape_html(&format!(":{name}:")))
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

        if let Some((html, consumed)) = render_emoji_shortcode(rest, context) {
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
    } else if raw.starts_with("!subteam^") {
        user_group_mention_html(raw, context)
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

fn user_group_mention_html(raw: &str, context: &MessageHtmlContext) -> String {
    let Some(group) = raw.strip_prefix("!subteam^") else {
        return escape_html(raw);
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

    if let Some(members) = context
        .user_group_members
        .get(group_id)
        .filter(|members| !members.is_empty())
    {
        format!(
            "<span class=\"mention\" title=\"{}\">@{}</span>",
            escape_html(&user_group_member_title(members)),
            escape_html(&label)
        )
    } else {
        format!("<span class=\"mention\">@{}</span>", escape_html(&label))
    }
}

fn normalized_user_group_label(label: &str) -> String {
    label.trim().trim_start_matches('@').to_string()
}

fn user_group_member_title(members: &[String]) -> String {
    gettext("Members: {members}").replace("{members}", &members.join(", "))
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

fn render_emoji_shortcode(text: &str, context: &MessageHtmlContext) -> Option<(String, usize)> {
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
    let emoji = EmojiCatalog::new(&context.custom_emojis).resolve(code);
    if debug::enabled() {
        debug::log(
            "render",
            &format!("emoji shortcode=:{code}: mapped={}", emoji.is_some()),
        );
    }

    let rendered = emoji
        .map(|emoji| {
            format!(
                "<span class=\"emoji\" title=\":{}:\" role=\"img\" aria-label=\"{}\">{}</span>",
                escape_html(code),
                escape_html(&code.replace(['_', '-'], " ")),
                emoji_value_html(&emoji, false),
            )
        })
        .unwrap_or_else(|| escape_html(shortcode));

    Some((rendered, end + 1))
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
    use crate::activity::{ActivityItem, ActivityKind};
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

    fn contrast_ratio(foreground: &str, background: &str) -> f64 {
        fn luminance(color: &str) -> f64 {
            let channel = |offset| {
                let value =
                    u8::from_str_radix(&color[offset..offset + 2], 16).unwrap() as f64 / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
        }

        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    #[test]
    fn normalizes_posix_locales_to_escaped_bcp_47_language_tags() {
        assert_eq!(normalize_language_tag("nl_NL.UTF-8"), Some("nl-NL".into()));
        assert_eq!(
            normalize_language_tag("sr_RS@latin"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(
            normalize_language_tag("sr_RS@cyrillic"),
            Some("sr-Cyrl-RS".into())
        );
        assert_eq!(
            normalize_language_tag("sr_Latn_RS@latin"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(
            normalize_language_tag("sr_Latn_RS@cyrillic"),
            Some("sr-Latn-RS".into())
        );
        assert_eq!(normalize_language_tag("C"), None);
        assert_eq!(normalize_language_tag("POSIX"), None);
        assert_eq!(normalize_language_tag("en\"><script>"), None);

        let html = html_document_with_language(
            "Messages",
            "<main></main>",
            None,
            "en\"><script>alert(1)</script>",
        );
        assert!(html.contains(
            "<html lang=\"en&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\" dir=\"ltr\">"
        ));
        assert!(!html.contains("<html lang=\"en\"><script>"));
    }

    #[test]
    fn document_root_direction_follows_the_normalized_primary_language() {
        for language in ["ar", "ar-EG", "he-IL"] {
            let html = html_document_with_language("Title", "<main></main>", None, language);
            assert!(
                html.contains(&format!("<html lang=\"{language}\" dir=\"rtl\">")),
                "{language}"
            );
        }

        let html = html_document_with_language("Title", "<main></main>", None, "en-GB");
        assert!(html.contains("<html lang=\"en-GB\" dir=\"ltr\">"));
    }

    #[test]
    fn document_root_direction_prefers_explicit_script_subtags() {
        for language in [
            "az-Arab", "ku-Arab", "pa-Arab", "en-Hebr", "ff-Adlm", "sd-Syrc", "dv-Thaa",
            "rhg-Rohg", "nqo-Nkoo",
        ] {
            assert_eq!(document_direction(language), "rtl", "{language}");
        }

        for language in ["ar-Latn", "he-Latn", "az-Latn", "ku-Cyrl"] {
            assert_eq!(document_direction(language), "ltr", "{language}");
        }
    }

    #[test]
    fn placeholder_document_preserves_and_escapes_prelocalized_inputs() {
        let html = placeholder_document("Titel & meer", "Runtime <error> & details");

        assert!(html.contains("<title>Titel &amp; meer</title>"));
        assert!(html.contains("Runtime &lt;error&gt; &amp; details"));
    }

    #[test]
    fn document_css_supports_touch_keyboard_motion_and_logical_layout() {
        let html = placeholder_document("Messages", "No messages");

        assert!(html.contains("--accent: #0061c9"));
        assert!(html.contains("--accent: #78aeff"));
        assert!(html.contains("flex-wrap: wrap"));
        assert!(html.contains("opacity: 1"));
        assert!(html.contains("@media (hover: hover) and (pointer: fine)"));
        assert!(html.contains(":focus-visible"));
        assert!(html.contains("@media (prefers-reduced-motion: reduce)"));
        assert!(html.contains("padding-inline:"));
        assert!(html.contains(".quick-actions {\n  position: static;"));
        let fine_pointer_css = html
            .split("@media (hover: hover) and (pointer: fine)")
            .nth(1)
            .unwrap()
            .split("@media (prefers-reduced-motion: reduce)")
            .next()
            .unwrap();
        assert!(!fine_pointer_css.contains("position: absolute"));
        assert!(!fine_pointer_css.contains("inset-"));
        assert!(fine_pointer_css.contains("grid-template-columns: minmax(0, 1fr) auto"));
        assert!(fine_pointer_css.contains(".message > :not(.quick-actions)"));
        assert!(fine_pointer_css.contains("grid-column: 1"));
        assert!(fine_pointer_css.contains("grid-row: 1"));
        assert!(fine_pointer_css.contains("grid-column: 2"));
        assert!(fine_pointer_css.contains("align-self: start"));
        for physical_property in [
            "padding-right:",
            "padding-left:",
            "margin-right:",
            "margin-left:",
            "right:",
            "left:",
        ] {
            assert!(!html.contains(physical_property), "{physical_property}");
        }
    }

    #[test]
    fn document_color_variables_meet_wcag_aa_for_normal_text() {
        for (foreground, background) in [
            ("#202124", "#fafafa"),
            ("#6a6f76", "#fafafa"),
            ("#0061c9", "#fafafa"),
            ("#0061c9", "#e5f1ff"),
            ("#f2f2f2", "#1d1f20"),
            ("#b6babf", "#1d1f20"),
            ("#78aeff", "#1d1f20"),
            ("#78aeff", "#183653"),
        ] {
            assert!(
                contrast_ratio(foreground, background) >= 4.5,
                "{foreground} on {background}"
            );
        }
    }

    #[test]
    fn escapes_message_text_and_author() {
        let mut message = message("hello <script>alert(1)</script> & goodbye");
        message.username = Some("<bad author>".to_string());

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("hello &lt;script&gt;alert(1)&lt;/script&gt; &amp; goodbye"));
        assert!(html.contains("&lt;bad author&gt;"));
        assert!(!html.contains("hello <script>"));
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
    fn resolves_user_group_mentions_with_member_tooltips() {
        let context = MessageHtmlContext {
            user_group_names: HashMap::from([("S123".to_string(), "platform".to_string())]),
            user_group_members: HashMap::from([(
                "S123".to_string(),
                vec!["Ada Lovelace".to_string(), "Grace Hopper".to_string()],
            )]),
            ..Default::default()
        };
        let html = conversation_document(
            "C123",
            &[message("Reminder: <!subteam^S123> please post updates")],
            &context,
        );

        assert!(html.contains(
            "<span class=\"mention\" title=\"Members: Ada Lovelace, Grace Hopper\">@platform</span>"
        ));
        assert!(!html.contains("!subteam^S123"));
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

        assert!(html.contains("title=\":rocket:\" role=\"img\""));
        assert!(html.contains(">🚀</span>"));
        assert!(html.contains("title=\":stuck_out_tongue:\" role=\"img\""));
        assert!(html.contains(">😛</span>"));
        assert!(html.contains(":unknown_custom_emoji:"));
    }

    #[test]
    fn renders_thread_emoji_shortcodes_embedded_in_text() {
        let context = MessageHtmlContext {
            thread_ts: Some("1710000000.000100".to_string()),
            ..Default::default()
        };
        let html = conversation_document(
            "C123",
            &[message("Something in a thread :troll:")],
            &context,
        );

        assert!(html.contains("title=\":troll:\" role=\"img\""));
        assert!(html.contains(">🧌</span>"));
    }

    #[test]
    fn workspace_emoji_are_shared_by_messages_quick_actions_and_picker() {
        let context = MessageHtmlContext {
            custom_emojis: HashMap::from([
                (
                    "party_parrot".to_string(),
                    "https://emoji.example/party-parrot.gif".to_string(),
                ),
                ("parrot_alias".to_string(), "alias:party_parrot".to_string()),
            ]),
            recent_reactions: vec!["party_parrot".to_string()],
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("dance :parrot_alias:")], &context);

        assert!(html.contains("title=\":parrot_alias:\" role=\"img\""));
        assert!(html.contains("src=\"https://emoji.example/party-parrot.gif\""));
        assert!(html.contains("name=party_parrot"));
        assert!(html.contains("data-emoji-name=\"party_parrot\""));
        assert!(html.contains("data-category=\"Workspace\""));
        assert!(html.contains("data-src=\"https://emoji.example/party-parrot.gif\""));
        assert!(html.contains("data-category=\"Flags\""));
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
    fn formats_timestamp_text_for_the_active_locale_and_keeps_iso_machine_time() {
        let (machine, full, short) = localized_timestamp_parts("1710000000.000100").unwrap();

        assert!(machine.contains('T'));
        assert!(!full.trim().is_empty());
        assert!(!short.trim().is_empty());
        assert!(localized_timestamp_parts("invalid").is_none());
    }

    #[test]
    fn conversation_messages_include_stable_scroll_anchors() {
        let html = conversation_document(
            "C123",
            &[message("anchored")],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains(
            "id=\"message-1710000000.000100\" tabindex=\"-1\" data-message-ts=\"1710000000.000100\""
        ));
        assert!(html.contains("<div class=\"body\" dir=\"auto\">"));
    }

    #[test]
    fn older_page_scroll_behavior_installs_prepend_preservation_script() {
        let context = MessageHtmlContext {
            timeline_scroll: TimelineScrollBehavior::PreservePrepend,
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("paged")], &context);

        assert!(html.contains("mode = \"preserve-prepend\""));
        assert!(html.contains("conduit:timeline-anchor:C123"));
    }

    #[test]
    fn renders_edited_message_metadata() {
        let mut message = message("edited text");
        message.edited = Some(crate::models::SlackMessageEdit {
            user: Some("U123".to_string()),
            ts: Some("1710000010.000100".to_string()),
        });

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("<span class=\"metadata\">edited</span>"));
        assert!(html.contains("edited text"));
    }

    #[test]
    fn renders_deleted_messages_as_deleted() {
        let mut message = message("");
        message.subtype = Some("message_deleted".to_string());

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("Message deleted"));
        assert!(!html.contains("No message text"));
    }

    #[test]
    fn groups_adjacent_messages_from_same_sender_under_three_minutes() {
        let messages = vec![
            message_at("U999", "other", "1710000300.000000"),
            message_at("U123", "second", "1710000100.000000"),
            message_at("U123", "first", "1710000000.000000"),
        ];

        let html = conversation_document("C123", &messages, &MessageHtmlContext::default());

        assert_eq!(
            html.matches("<article class=\"message message-group\"")
                .count(),
            2
        );
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

        assert_eq!(
            html.matches("<article class=\"message message-group\"")
                .count(),
            2
        );
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
    fn renders_block_action_urls_as_external_links() {
        let mut message = message("actions");
        message.blocks = Some(serde_json::json!([
            {
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Open canvas" },
                        "url": "https://example.slack.com/canvas/C123"
                    },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Callback only" },
                        "action_id": "callback"
                    },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Unsafe" },
                        "url": "javascript:alert(1)"
                    }
                ]
            }
        ]));

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html
            .contains("<a class=\"block-action\" href=\"https://example.slack.com/canvas/C123\""));
        assert!(html.contains(">Open canvas</a>"));
        assert!(html.contains("<span class=\"block-action\">Callback only</span>"));
        assert!(html.contains("<span class=\"block-action\">Unsafe</span>"));
        assert!(!html.contains("javascript:alert"));
    }

    #[test]
    fn renders_message_quick_actions() {
        let mut message = message("threaded");
        message.reply_count = Some(3);
        message.is_starred = Some(true);
        message.reactions = Some(vec![SlackReaction {
            name: Some("thumbsup".to_string()),
            count: Some(1),
            users: Some(vec!["U999".to_string()]),
        }]);
        let context = MessageHtmlContext {
            current_user_id: Some("U999".to_string()),
            user_names: HashMap::from([("U999".to_string(), "Ada Lovelace".to_string())]),
            recent_reactions: vec![
                "heart".to_string(),
                "thumbsup".to_string(),
                "eyes".to_string(),
            ],
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("conduit://thread?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains(">💬</a>"));
        assert!(html.contains(">thread (3)</a>"));
        assert!(html.contains(
            "conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=heart&amp;add=true"
        ));
        assert!(html.contains("conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=thumbsup&amp;add=false"));
        assert!(html.contains(
            "conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=eyes&amp;add=true"
        ));
        let reaction_chip = html.find("<span class=\"reaction is-active\"").unwrap();
        assert!(html.contains("title=\"Reacted: Ada Lovelace\""));
        assert!(html.contains("aria-label=\"Reacted: Ada Lovelace\""));
        let thread_chip = html.find("<a class=\"reaction thread-reaction\"").unwrap();
        assert!(reaction_chip < thread_chip);
        assert!(html.contains("conduit://save?channel=C123&amp;ts=1710000000.000100&amp;add=false"));
        assert!(html.contains("conduit://copy-link?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("conduit://copy-message?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("conduit://forward?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("data-open-reaction-picker"));
        assert_eq!(html.matches("id=\"reaction-picker\"").count(), 1);
        assert!(html.contains("id=\"emoji-search\""));
        assert!(html.contains("role=\"tablist\""));
        assert!(html.contains("class=\"emoji-grid\""));
        assert!(html.contains("role=\"menu\""));
        assert!(html.contains("if (menu) menu.open = false"));
        let quick_actions = &html[html.find("<nav class=\"quick-actions\"").unwrap()..];
        let quick_actions = &quick_actions[..quick_actions.find("</nav>").unwrap()];
        let recent = quick_actions.find("name=heart").unwrap();
        let picker = quick_actions.find("data-open-reaction-picker").unwrap();
        let thread = quick_actions.find("conduit://thread?").unwrap();
        let forward = quick_actions.find("conduit://forward?").unwrap();
        let more = quick_actions.find("class=\"more-actions\"").unwrap();
        assert!(recent < picker && picker < thread && thread < forward && forward < more);
        for unavailable_action in [
            "Edit message",
            "Mark unread",
            "Remind me",
            "Turn off notifications",
            "Organise",
            "Connect to apps",
            "Delete message",
        ] {
            assert!(!html.contains(unavailable_action), "{unavailable_action}");
        }
        assert!(!html.contains("Remove +1"));
    }

    #[test]
    fn unread_conversation_marks_read_only_when_bottom_sentinel_is_visible() {
        let context = MessageHtmlContext {
            read_marker_url: Some(mark_read_action_url("C123", "1710000000.000100")),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("unread")], &context);

        assert!(html.contains("id=\"conversation-read-sentinel\""));
        assert!(html.contains("new IntersectionObserver"));
        assert!(html.contains(
            "window.location.href = \"conduit://mark-read?channel=C123&ts=1710000000.000100\""
        ));
    }

    #[test]
    fn renders_channel_load_more_action_before_messages() {
        let context = MessageHtmlContext {
            load_more_url: Some(load_more_action_url("C123", "next cursor", None)),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("paged")], &context);
        let load_more = html.find("Load older messages").unwrap();
        let message = html.find("paged").unwrap();

        assert!(load_more < message);
        assert!(html.contains("conduit://load-older?channel=C123&amp;cursor=next%20cursor"));
    }

    #[test]
    fn renders_thread_load_more_action_after_replies() {
        let context = MessageHtmlContext {
            thread_ts: Some("1710000000.000100".to_string()),
            load_more_url: Some(load_more_action_url(
                "C123",
                "next cursor",
                Some("1710000000.000100"),
            )),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("reply")], &context);
        let reply = html.find("reply").unwrap();
        let load_more = html.find("Load more replies").unwrap();

        assert!(reply < load_more);
        assert!(html.contains("thread_ts=1710000000.000100"));
    }

    #[test]
    fn thread_context_actions_reload_thread_without_thread_button() {
        let image_url = "https://files.slack.com/files-pri/T123-F123/thread.png";
        let mut message = message("reply");
        message.text = Some("reply :stuck_out_tongue:".to_string());
        message.files = Some(vec![SlackFile {
            title: Some("Thread image".to_string()),
            mimetype: Some("image/png".to_string()),
            thumb_480: Some(image_url.to_string()),
            ..Default::default()
        }]);
        message.thread_ts = Some("1710000000.000100".to_string());
        message.ts = "1710000010.000200".to_string();
        let context = MessageHtmlContext {
            thread_ts: Some("1710000000.000100".to_string()),
            image_assets: HashMap::from([(
                image_url.to_string(),
                "data:image/png;base64,thread".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("thread_ts=1710000000.000100"));
        assert!(!html.contains("conduit://thread?"));
        assert!(html.contains("title=\":stuck_out_tongue:\" role=\"img\""));
        assert!(html.contains("src=\"data:image/png;base64,thread\""));
        assert!(html.contains("Thread image"));
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
    fn image_attachment_opens_original_media_in_internal_viewer() {
        let preview = "https://files.slack.com/preview.png";
        let original = "https://files.slack.com/original.png";
        let mut message = message("image");
        message.files = Some(vec![SlackFile {
            title: Some("Diagram".to_string()),
            mimetype: Some("image/png".to_string()),
            url_private_download: Some(original.to_string()),
            thumb_480: Some(preview.to_string()),
            ..Default::default()
        }]);

        let context = MessageHtmlContext {
            image_assets: HashMap::from([(
                preview.to_string(),
                "data:image/png;base64,x".to_string(),
            )]),
            ..Default::default()
        };
        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("conduit://media?"));
        assert!(html.contains("url=https%3A%2F%2Ffiles.slack.com%2Foriginal.png"));
        assert!(html.contains("src=\"data:image/png;base64,x\""));
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
    fn unreads_document_renders_rows() {
        let items = vec![ActivityItem {
            channel_id: "C123".to_string(),
            title: "#general & friends".to_string(),
            kind: ActivityKind::PublicChannel,
            unread: true,
            unread_count: 3,
        }];

        let html = unreads_document(&items);

        assert!(html.contains("<main class=\"timeline\" aria-labelledby=\"document-title\">"));
        assert!(html.contains("<ul class=\"activity-list\"><li>"));
        assert!(html.contains("class=\"activity-title\" dir=\"auto\""));
        assert!(html.contains("#general &amp; friends"));
        assert!(html.contains("3 unread"));
        assert!(html.contains("Channel"));
        assert!(html.contains("conduit://unreads-open?channel=C123"));
    }

    #[test]
    fn unreads_document_uses_empty_state_without_rows() {
        let html = unreads_document(&[]);

        assert!(html.contains("No unread conversations"));
        assert!(!html.contains("<a class=\"activity-row\""));
    }

    #[test]
    fn files_document_renders_file_rows() {
        let files = vec![SlackFile {
            title: Some("Quarterly <plan>.pdf".to_string()),
            pretty_type: Some("PDF".to_string()),
            size: Some(1_048_576),
            permalink: Some("https://slack.example/files/F123".to_string()),
            ..Default::default()
        }];

        let html = files_document(&files);

        assert!(html.contains("<main class=\"timeline\" aria-labelledby=\"document-title\">"));
        assert!(html.contains("<ul class=\"file-list\"><li>"));
        assert!(html.contains("class=\"file-title\" dir=\"auto\""));
        assert!(html.contains("Quarterly &lt;plan&gt;.pdf"));
        assert!(html.contains("PDF - 1.0 MB"));
        assert!(html.contains("href=\"https://slack.example/files/F123\""));
    }

    #[test]
    fn files_document_uses_empty_state_without_rows() {
        let html = files_document(&[]);

        assert!(html.contains("No files"));
        assert!(!html.contains("<a class=\"file-row\""));
        assert!(!html.contains("<section class=\"file-row\""));
    }

    #[test]
    fn threads_document_links_roots_to_existing_thread_navigation() {
        let mut root = message("A useful thread");
        root.reply_count = Some(3);
        let html = threads_document(
            &[ThreadInboxItem {
                channel_id: "C123".to_string(),
                channel_title: "general".to_string(),
                root,
            }],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains("general · 3 replies"));
        assert!(html.contains("conduit://thread?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("A useful thread"));
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

    #[test]
    fn search_results_link_to_valid_internal_message_locations() {
        let results = vec![SearchMatch {
            channel: Some(crate::models::SlackSearchChannel {
                id: Some("C 123".to_string()),
                name: Some("general".to_string()),
            }),
            ts: Some("1710000001.000100".to_string()),
            thread_ts: Some("1710000000.000100".to_string()),
            permalink: Some("https://example.slack.com/archives/C123/p1".to_string()),
            ..Default::default()
        }];

        let html = search_results_document(&results, &MessageHtmlContext::default());

        assert!(html.contains("Open in Conduit"));
        assert!(html.contains(
            "conduit://message?channel=C%20123&amp;ts=1710000001.000100&amp;thread_ts=1710000000.000100"
        ));
        assert!(html.contains("Open in Slack"));
    }

    #[test]
    fn search_results_omit_internal_link_without_a_complete_location() {
        let html = search_results_document(
            &[SearchMatch {
                ts: Some("1710000001.000100".to_string()),
                permalink: Some("https://example.slack.com/archives/C123/p1".to_string()),
                ..Default::default()
            }],
            &MessageHtmlContext::default(),
        );

        assert!(!html.contains("Open in Conduit"));
        assert!(html.contains("Open in Slack"));
    }

    #[test]
    fn focused_conversation_document_escapes_target_and_uses_static_script() {
        let target = "1710000000.000100\"</script><script>alert(1)</script>";
        let html = conversation_document_with_focus(
            "C123",
            &[SlackMessage {
                ts: target.to_string(),
                text: Some("focused".to_string()),
                ..Default::default()
            }],
            &MessageHtmlContext::default(),
            Some(target),
        );

        assert!(html.contains("data-focus-message-ts="));
        assert!(html.contains("&quot;&lt;/script&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("const targetTs = \"1710000000"));
        assert!(html.contains("timeline.dataset.focusMessageTs"));
        assert!(html.contains("target.scrollIntoView"));
        assert!(html.contains("target.focus"));
    }

    #[test]
    fn search_and_saved_documents_use_semantic_message_lists_and_time() {
        let results = vec![SearchMatch {
            username: Some("Ada".to_string()),
            text: Some("A result".to_string()),
            ts: Some("1710000000.000100".to_string()),
            ..Default::default()
        }];
        let search = search_results_document(&results, &MessageHtmlContext::default());
        assert!(search.contains("<ol class=\"message-list\"><li class=\"message-list-item\">"));
        assert!(search.contains("<time class=\"metadata\""));
        assert!(search.contains("id=\"message-1710000000.000100\" tabindex=\"-1\""));

        let saved = saved_items_document(
            &[SavedItem {
                channel: Some("C123".to_string()),
                message: Some(message("saved")),
                ..Default::default()
            }],
            &MessageHtmlContext::default(),
        );
        assert!(saved.contains("<ol class=\"message-list\"><li class=\"message-list-item\">"));
        assert!(saved.contains("<time class=\"metadata\""));
    }
}
