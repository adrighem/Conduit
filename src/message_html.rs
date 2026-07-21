use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use gettextrs::gettext;
use serde::Serialize;

use crate::activity::ActivityItem;
use crate::debug;
use crate::emoji::{
    emoji_picker_accessible_label, EmojiCatalog, EmojiEntry, EmojiPickerModel, EmojiValue,
};
use crate::models::{
    SavedItem, SearchMatch, SearchMessageLocation, SlackFile, SlackMessage, SlackUser,
    SlackUserStatus,
};

const MESSAGE_BASE_URI: &str = "app://conduit/messages/";
const DEFAULT_DOCUMENT_LANGUAGE: &str = "en";
pub(crate) const MESSAGE_BASE_FONT_SIZE_CSS_PX: f64 = 14.0;
const TIMESTAMP_LOCALIZATION_SCRIPT: &str = include_str!("timestamp_localization.js");
static TIME_FORMAT_LOCALE: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub struct MessageHtmlContext {
    pub user_names: HashMap<String, String>,
    pub user_full_names: HashMap<String, String>,
    pub user_avatar_urls: HashMap<String, String>,
    pub conversation_titles: HashMap<String, String>,
    pub user_statuses: HashMap<String, SlackUserStatus>,
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
    pub first_unread_ts: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimelineScrollBehavior {
    #[default]
    Preserve,
    PreservePrepend,
    Bottom,
    StickToBottom,
}

/// A small, typed command understood by the timeline's incremental DOM runtime.
///
/// Patch HTML is always produced by this module, so message contents pass through
/// the same escaping and URL validation as a complete document render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum TimelineDomPatch {
    InsertMessage {
        position: TimelineInsertPosition,
        html: String,
    },
    ReplaceMessage {
        message_ts: String,
        html: String,
        part_html: String,
    },
    RemoveMessage {
        message_ts: String,
    },
    ReplaceRegion {
        message_ts: String,
        region: TimelineMessageRegion,
        html: String,
    },
    UpdateImage {
        asset_key: String,
        source: Option<String>,
    },
    UpdateUser {
        user_id: String,
        name: String,
        status_html: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum TimelineInsertPosition {
    Append,
    Prepend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum TimelineMessageRegion {
    Body,
    Attachments,
    Responses,
}

/// Render a message into a patch command. Incrementally inserted messages are
/// intentionally standalone; the next complete render may visually regroup them.
#[allow(dead_code)]
pub fn insert_message_patch(
    channel_id: &str,
    message: &SlackMessage,
    context: &MessageHtmlContext,
    position: TimelineInsertPosition,
) -> TimelineDomPatch {
    let unread_separator = (context.first_unread_ts.as_deref() == Some(message.ts.as_str()))
        .then(unread_separator_html)
        .unwrap_or_default();
    TimelineDomPatch::InsertMessage {
        position,
        html: format!(
            "{unread_separator}<li class=\"message-list-item\">{}</li>",
            message_article(Some(channel_id), message, context)
        ),
    }
}

#[allow(dead_code)]
pub fn replace_message_patch(
    channel_id: &str,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> TimelineDomPatch {
    TimelineDomPatch::ReplaceMessage {
        message_ts: message.ts.clone(),
        html: message_article(Some(channel_id), message, context),
        part_html: message_part_html(Some(channel_id), message, context),
    }
}

#[allow(dead_code)]
pub fn remove_message_patch(message_ts: impl Into<String>) -> TimelineDomPatch {
    TimelineDomPatch::RemoveMessage {
        message_ts: message_ts.into(),
    }
}

#[allow(dead_code)]
pub fn message_region_patch(
    channel_id: &str,
    message: &SlackMessage,
    context: &MessageHtmlContext,
    region: TimelineMessageRegion,
) -> TimelineDomPatch {
    let html = match region {
        TimelineMessageRegion::Body => format!(
            "<div class=\"body\" dir=\"auto\">{}</div>",
            message_body_html(message, context)
        ),
        TimelineMessageRegion::Attachments => attachments_html(Some(channel_id), message, context),
        TimelineMessageRegion::Responses => {
            message_responses_html(Some(channel_id), message, context)
        }
    };
    TimelineDomPatch::ReplaceRegion {
        message_ts: message.ts.clone(),
        region,
        html,
    }
}

#[allow(dead_code)]
pub fn update_image_patch(
    asset_key: impl Into<String>,
    source: Option<String>,
) -> TimelineDomPatch {
    TimelineDomPatch::UpdateImage {
        asset_key: asset_key.into(),
        source,
    }
}

#[allow(dead_code)]
pub fn update_user_patch(
    user_id: impl Into<String>,
    name: impl Into<String>,
    status: Option<&SlackUserStatus>,
    custom_emojis: &HashMap<String, String>,
) -> TimelineDomPatch {
    TimelineDomPatch::UpdateUser {
        user_id: user_id.into(),
        name: name.into(),
        status_html: status
            .filter(|status| status.active_at(current_unix_seconds()))
            .map(|status| user_status_html(status, custom_emojis))
            .unwrap_or_default(),
    }
}

/// JavaScript suitable for `WebView::evaluate_javascript`.
#[allow(dead_code)]
pub fn timeline_dom_patch_call(patch: &TimelineDomPatch) -> String {
    let payload = serde_json::to_string(patch)
        .expect("timeline DOM patch should serialize")
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!(
        "window.conduitApplyTimelinePatch ? window.conduitApplyTimelinePatch({payload}) : false;"
    )
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
    if locale.is_empty() || locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX")
    {
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
        Some("arabic") => Some("Arab"),
        Some("cyrillic" | "cyrl") => Some("Cyrl"),
        Some("devanagari") => Some("Deva"),
        Some("hebrew") => Some("Hebr"),
        Some("ijekavianlatin" | "iqtelif" | "latin" | "latn") => Some("Latn"),
        Some("shaw") => Some("Shaw"),
        _ => None,
    };
    let modifier_variant = match modifier.as_deref() {
        Some("ije" | "ijekavian" | "ijekavianlatin") => Some("ijekavsk"),
        Some("valencia") => Some("valencia"),
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
    if let Some(variant) = modifier_variant.filter(|variant| {
        !normalized
            .iter()
            .any(|subtag| subtag.eq_ignore_ascii_case(variant))
    }) {
        normalized.push(variant.to_string());
    }

    Some(normalized.join("-"))
}

fn document_language() -> String {
    gtk::glib::language_names()
        .iter()
        .find_map(|language| normalize_language_tag(language.as_str()))
        .unwrap_or_else(|| DEFAULT_DOCUMENT_LANGUAGE.to_string())
}

pub(crate) fn initialize_time_format_locale(locale: Option<&[u8]>) {
    let _ = TIME_FORMAT_LOCALE.set(normalize_time_format_locale(locale));
}

fn normalize_time_format_locale(locale: Option<&[u8]>) -> Option<String> {
    locale
        .and_then(|locale| std::str::from_utf8(locale).ok())
        .and_then(normalize_language_tag)
}

fn configured_time_locale() -> Option<&'static str> {
    TIME_FORMAT_LOCALE
        .get_or_init(time_format_locale_from_environment)
        .as_deref()
}

fn time_format_locale_from_environment() -> Option<String> {
    let lc_all = std::env::var("LC_ALL").ok();
    let lc_time = std::env::var("LC_TIME").ok();
    let lang = std::env::var("LANG").ok();
    preferred_time_locale([lc_all.as_deref(), lc_time.as_deref(), lang.as_deref()])
}

fn preferred_time_locale<'a>(locales: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    for locale in locales.into_iter().flatten() {
        if !locale.trim().is_empty() {
            return normalize_language_tag(locale);
        }
    }
    None
}

fn document_heading(title: &str) -> String {
    format!(
        "<h1 id=\"document-title\" class=\"visually-hidden\">{}</h1>",
        escape_html(title)
    )
}

fn emoji_picker_html(context: &MessageHtmlContext) -> String {
    let catalog = EmojiCatalog::new(&context.custom_emojis);
    let model = EmojiPickerModel::new(catalog.entries());
    let entries = model.entries();
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
        .enumerate()
        .map(|(index, emoji)| {
            format!(
                "<button id=\"emoji-choice-{index}\" type=\"button\" class=\"emoji-choice\" role=\"gridcell\" tabindex=\"-1\" aria-selected=\"false\" data-emoji-name=\"{}\" data-emoji-label=\"{}\" data-category=\"{}\" data-original-index=\"{}\" title=\":{}:\" aria-label=\"{}\">{}</button>",
                escape_html(&emoji.name),
                escape_html(&emoji.label),
                escape_html(emoji.category),
                index,
                escape_html(&emoji.name),
                escape_html(&emoji_picker_accessible_label(emoji)),
                emoji_value_html(&emoji.value, true),
            )
        })
        .collect::<String>();

    format!(
        "<dialog id=\"emoji-picker\" class=\"emoji-picker\" aria-labelledby=\"emoji-picker-title\"><header><h2 id=\"emoji-picker-title\">{}</h2><button type=\"button\" class=\"picker-close\" aria-label=\"{}\">×</button></header><label class=\"emoji-search-label\" for=\"emoji-search\">{}</label><input id=\"emoji-search\" class=\"emoji-search\" type=\"search\" role=\"combobox\" aria-controls=\"emoji-grid\" aria-expanded=\"true\" autocomplete=\"off\" placeholder=\"{}\"><nav class=\"emoji-categories\" role=\"tablist\" aria-label=\"{}\">{category_buttons}</nav><div id=\"emoji-grid\" class=\"emoji-grid\" role=\"grid\" aria-label=\"{}\">{emoji_buttons}</div><p class=\"emoji-empty\" role=\"status\" hidden>{}</p></dialog>",
        escape_html(&gettext("Add reaction")),
        escape_html(&gettext("Close emoji picker")),
        escape_html(&gettext("Search emoji by name")),
        escape_html(&gettext("Search emoji")),
        escape_html(&gettext("Emoji categories")),
        escape_html(&gettext("Emoji")),
        escape_html(&gettext("No emoji found")),
    )
}

fn emoji_picker_script() -> &'static str {
    r##"(function () {
  const picker = document.getElementById("emoji-picker");
  if (!picker) return;
  const search = picker.querySelector("#emoji-search");
  const choices = Array.from(picker.querySelectorAll(".emoji-choice"));
  const grid = picker.querySelector(".emoji-grid");
  const categories = picker.querySelector(".emoji-categories");
  const tabs = Array.from(picker.querySelectorAll("[data-emoji-category]"));
  const empty = picker.querySelector(".emoji-empty");
  let activeCategory = "Smileys";
  let reactionTemplate = "";
  let opener = null;
  let selectedChoice = null;

  function visibleChoices() {
    return choices.filter(function (choice) { return !choice.hidden; });
  }

  function selectChoice(choice, focus) {
    selectedChoice = choice || null;
    choices.forEach(function (item) {
      const selected = item === selectedChoice;
      item.setAttribute("aria-selected", String(selected));
      item.tabIndex = selected ? 0 : -1;
    });
    if (selectedChoice) {
      search.setAttribute("aria-activedescendant", selectedChoice.id);
      selectedChoice.scrollIntoView({ block: "nearest" });
      if (focus) selectedChoice.focus();
    } else {
      search.removeAttribute("aria-activedescendant");
    }
  }

  function moveSelection(offset) {
    const visible = visibleChoices();
    if (visible.length === 0) return;
    const current = Math.max(0, visible.indexOf(selectedChoice));
    const next = Math.max(0, Math.min(visible.length - 1, current + offset));
    selectChoice(visible[next], false);
  }

  function activateChoice(choice) {
    if (!choice) return;
    const url = reactionTemplate.replace("__REACTION__", encodeURIComponent(choice.dataset.emojiName));
    picker.close();
    window.location.href = url;
  }

  function fieldTokens(term, value) {
    const normalized = value.toLocaleLowerCase();
    const alphanumericTerm = Array.from(term).every(function (character) {
      return /[\p{L}\p{N}]/u.test(character);
    });
    return alphanumericTerm ? normalized.match(/[\p{L}\p{N}]+/gu) || [] : [normalized];
  }

  function termFieldScore(term, value, fieldWeight) {
    return fieldTokens(term, value).reduce(function (best, token) {
      const position = token.indexOf(term);
      if (position < 0) return best;
      const termLength = Array.from(term).length;
      const tokenLength = Math.max(Array.from(token).length, 1);
      const matchScore = token === term
        ? 100
        : position === 0
          ? Math.min(90, 50 + termLength * 10)
          : Math.floor(Math.floor(termLength * 100 / tokenLength) * 75 / 100);
      const score = Math.floor(matchScore * fieldWeight / 100);
      return Math.max(best, score);
    }, -1);
  }

  function choiceMatchScore(choice, terms) {
    if (terms.length === 0) return 0;
    const termScores = terms.map(function (term) {
      return Math.max(
        termFieldScore(term, choice.dataset.emojiName, 100),
        termFieldScore(term, choice.dataset.emojiLabel, 85)
      );
    });
    if (termScores.some(function (score) { return score < 0; })) return null;
    const mean = Math.floor(termScores.reduce(function (sum, score) { return sum + score; }, 0) / termScores.length);
    return Math.floor((70 * mean + 30 * Math.min(...termScores)) / 100);
  }

  function filterChoices() {
    const terms = search.value.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    categories.hidden = terms.length > 0;
    let visible = 0;
    choices.forEach(function (choice) {
      const score = choiceMatchScore(choice, terms);
      const matchesQuery = score !== null;
      const matchesCategory = terms.length > 0 || choice.dataset.category === activeCategory;
      choice.hidden = !(matchesQuery && matchesCategory);
      choice.dataset.matchBand = String(Math.min(Math.floor((score || 0) / 5), 19));
      if (!choice.hidden) {
        const image = choice.querySelector("img[data-src]");
        if (image) {
          image.src = image.dataset.src;
          image.removeAttribute("data-src");
        }
        visible += 1;
      }
    });
    choices
      .slice()
      .sort(function (left, right) {
        if (left.hidden !== right.hidden) return left.hidden ? 1 : -1;
        if (terms.length > 0 && !left.hidden) {
          const bandDifference = Number(right.dataset.matchBand) - Number(left.dataset.matchBand);
          if (bandDifference !== 0) return bandDifference;
        }
        return Number(left.dataset.originalIndex) - Number(right.dataset.originalIndex);
      })
      .forEach(function (choice) { grid.appendChild(choice); });
    empty.hidden = visible !== 0;
    selectChoice(visibleChoices()[0] || null, false);
  }

  function cancelPicker(event) {
    if (event) {
      event.preventDefault();
      event.stopPropagation();
    }
    if (picker.open) picker.close("cancel");
  }

  document.addEventListener("click", function (event) {
    const menuAction = event.target.closest(".more-actions-menu a");
    if (menuAction) {
      const menu = menuAction.closest("details");
      if (menu) menu.open = false;
    }
    const trigger = event.target.closest("[data-open-emoji-picker]");
    if (!trigger) return;
    event.preventDefault();
    opener = trigger;
    reactionTemplate = trigger.dataset.reactionTemplate;
    search.value = "";
    filterChoices();
    picker.showModal();
    search.focus();
  });

  picker.querySelector(".picker-close").addEventListener("click", cancelPicker);
  picker.addEventListener("cancel", cancelPicker);
  document.addEventListener("keydown", function (event) {
    if (!picker.open || (event.key !== "Escape" && event.key !== "Esc")) return;
    cancelPicker(event);
  }, true);
  picker.addEventListener("keydown", function (event) {
    if (event.key === "ArrowUp" || event.key === "ArrowDown") {
      event.preventDefault();
      event.stopPropagation();
      moveSelection(event.key === "ArrowUp" ? -1 : 1);
    } else if (event.key === "Enter" && selectedChoice) {
      event.preventDefault();
      event.stopPropagation();
      activateChoice(selectedChoice);
    }
  }, true);
  picker.addEventListener("click", function (event) {
    if (event.target !== picker) return;
    const bounds = picker.getBoundingClientRect();
    const inside = event.clientX >= bounds.left && event.clientX <= bounds.right
      && event.clientY >= bounds.top && event.clientY <= bounds.bottom;
    if (!inside) cancelPicker(event);
  });
  picker.addEventListener("close", function () { if (opener) opener.focus(); });
  search.addEventListener("input", filterChoices);
  tabs.forEach(function (tab) {
    tab.addEventListener("click", function () {
      activeCategory = tab.dataset.emojiCategory;
      tabs.forEach(function (item) { item.setAttribute("aria-selected", String(item === tab)); });
      search.value = "";
      filterChoices();
      const first = choices.find(function (choice) { return !choice.hidden; });
      if (first) selectChoice(first, true);
    });
  });
  choices.forEach(function (choice) {
    choice.addEventListener("click", function () {
      activateChoice(choice);
    });
  });
})();"##
}

fn author_actions_script() -> &'static str {
    r#"(function () {
  function closeAuthorMenus(except) {
    document.querySelectorAll("details.author-actions[open]").forEach(function (menu) {
      if (menu !== except) menu.open = false;
    });
  }

  function closeMentionMenus(except) {
    document.querySelectorAll(".mention-actions > button[aria-expanded='true']").forEach(function (button) {
      if (button === except) return;
      button.setAttribute("aria-expanded", "false");
      button.nextElementSibling.hidden = true;
    });
  }

  document.addEventListener("click", function (event) {
    const mention = event.target.closest(".mention-actions > button");
    if (mention) {
      const opening = mention.getAttribute("aria-expanded") !== "true";
      closeAuthorMenus(null);
      closeMentionMenus(mention);
      mention.setAttribute("aria-expanded", opening ? "true" : "false");
      mention.nextElementSibling.hidden = !opening;
      return;
    }
    const author = event.target.closest("details.author-actions");
    if (!author) closeAuthorMenus(null);
    if (!event.target.closest(".mention-actions")) closeMentionMenus(null);
  });

  document.addEventListener("keydown", function (event) {
    if (event.key !== "Escape" && event.key !== "Esc") return;
    const authorMenu = document.querySelector("details.author-actions[open]");
    const mention = document.querySelector(".mention-actions > button[aria-expanded='true']");
    if (!authorMenu && !mention) return;
    event.preventDefault();
    if (authorMenu) {
      authorMenu.open = false;
      const author = authorMenu.querySelector("summary");
      if (author) author.focus();
    }
    if (mention) {
      mention.setAttribute("aria-expanded", "false");
      mention.nextElementSibling.hidden = true;
      mention.focus();
    }
  }, true);
})();"#
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

pub fn user_profile_document(user: &SlackUser, context: &MessageHtmlContext) -> String {
    let profile = user.profile.as_ref();
    let display_name = user
        .display_name()
        .unwrap_or_else(|| gettext("Unknown person"));
    let full_name = profile
        .and_then(|profile| profile.real_name.as_deref())
        .or(user.real_name.as_deref())
        .unwrap_or(&display_name);
    let image = profile.and_then(|profile| {
        profile
            .image_original
            .as_deref()
            .or(profile.image_512.as_deref())
            .or(profile.image_192.as_deref())
            .or(profile.image_72.as_deref())
    });
    let mut body = format!(
        "<main class=\"profile-page\" aria-labelledby=\"document-title\"><nav><a href=\"conduit://profile-close\">← {}</a></nav><header class=\"profile-header\">",
        profile_text_html(&gettext("Back to conversation"), context)
    );
    if let Some(image) = image.filter(|url| is_http_url(url)) {
        body.push_str(&format!(
            "<img class=\"profile-picture\" src=\"{}\" alt=\"{}\">",
            escape_html(image),
            escape_html(&format!("{} profile picture", full_name))
        ));
    }
    body.push_str(&format!(
        "<div><h1 id=\"document-title\">{}</h1><p class=\"profile-full-name\">{}</p></div></header><dl class=\"profile-details\">",
        profile_text_html(&display_name, context), profile_text_html(full_name, context)
    ));
    let mut detail = |label: &str, value: Option<&str>| {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            body.push_str(&format!(
                "<div><dt>{}</dt><dd dir=\"auto\">{}</dd></div>",
                profile_text_html(label, context),
                profile_text_html(value, context)
            ));
        }
    };
    if let Some(status) = user.status() {
        let status_value = [status.emoji.as_str(), status.text.as_str()]
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        detail(&gettext("Status"), Some(&status_value));
        if status.expiration > 0 {
            let expiration = gtk::glib::DateTime::from_unix_local(status.expiration)
                .ok()
                .and_then(|datetime| datetime.format("%c %Z").ok())
                .map(|value| value.to_string())
                .unwrap_or_else(|| status.expiration.to_string());
            detail(&gettext("Status expiration"), Some(&expiration));
        }
    }
    detail(
        &gettext("Job title"),
        profile.and_then(|p| p.title.as_deref()),
    );
    detail(
        &gettext("Pronouns"),
        profile.and_then(|p| p.pronouns.as_deref()),
    );
    detail(&gettext("Email"), profile.and_then(|p| p.email.as_deref()));
    detail(&gettext("Phone"), profile.and_then(|p| p.phone.as_deref()));
    detail(&gettext("Skype"), profile.and_then(|p| p.skype.as_deref()));
    detail(&gettext("About"), profile.and_then(|p| p.about.as_deref()));
    detail(
        &gettext("Location"),
        profile
            .and_then(|p| p.location.as_deref())
            .or(user.tz_label.as_deref()),
    );
    detail(&gettext("Time zone"), user.tz.as_deref());
    if let Some(profile) = profile {
        let mut fields = profile.fields.iter().collect::<Vec<_>>();
        fields.sort_by(|(left_id, left), (right_id, right)| {
            left.label
                .as_deref()
                .unwrap_or(left_id)
                .cmp(right.label.as_deref().unwrap_or(right_id))
        });
        for (field_id, field) in fields {
            detail(
                field.label.as_deref().unwrap_or(field_id),
                field.display_value(),
            );
        }
    }
    body.push_str("</dl></main>");
    html_document(&display_name, &body)
}

fn profile_text_html(text: &str, context: &MessageHtmlContext) -> String {
    let mut output = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((_, consumed)) = decode_html_entity_prefix(rest) {
            output.push_str(&escape_html(&rest[..consumed]));
            rest = &rest[consumed..];
            continue;
        }
        if let Some((html, consumed)) = render_emoji_shortcode(rest, context) {
            output.push_str(&html);
            rest = &rest[consumed..];
            continue;
        }
        let next = rest
            .chars()
            .next()
            .expect("non-empty string has a character");
        if next == '\n' {
            output.push_str("<br>");
        } else {
            output.push_str(&escape_html(&next.to_string()));
        }
        rest = &rest[next.len_utf8()..];
    }
    output
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

    let groups = message_groups(messages, context.first_unread_ts.as_deref());
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
        if group
            .first()
            .is_some_and(|message| context.first_unread_ts.as_deref() == Some(message.ts.as_str()))
        {
            body.push_str(&unread_separator_html());
        }
        body.push_str("<li class=\"message-list-item\">");
        body.push_str(&message_group_article(Some(channel_id), &group, context));
        body.push_str("</li>");
    }
    body.push_str("</ol>");
    if context.read_marker_url.is_some()
        && context.first_unread_ts.is_none()
        && context.thread_ts.is_some()
    {
        body.push_str("<div id=\"timeline-read-sentinel\" aria-hidden=\"true\"></div>");
    }
    if context.thread_ts.is_some() {
        if let Some(url) = context.load_more_url.as_deref() {
            body.push_str(&load_more_action_html(url, &gettext("Load more replies")));
        }
    }
    body.push_str("</main>");
    body.push_str(&emoji_picker_html(context));

    let mut scripts = vec![timeline_dom_runtime_script().to_string()];
    let scroll_identity = timeline_scroll_identity(channel_id, context.thread_ts.as_deref());
    if let Some(scroll_script) = timeline_scroll_script(&scroll_identity, context.timeline_scroll) {
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
        body.push_str(&emoji_picker_html(context));
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
            &gettext("No threads have been discovered yet"),
        );
    }

    let title = gettext("Threads");
    let mut body = format!(
        "<main class=\"timeline\" aria-labelledby=\"document-title\">{}<ol class=\"message-list\">",
        document_heading(&title)
    );
    for item in items {
        let reply_count = item.root.reply_count.unwrap_or_default();
        let mut label = gettext("{channel} · {count} replies")
            .replace("{channel}", &item.channel_title)
            .replace("{count}", &reply_count.to_string());
        if let Some(unread_count) = item.root.unread_count.filter(|count| *count > 0) {
            label.push_str(
                &gettext(" · {count} unread").replace("{count}", &unread_count.to_string()),
            );
        }
        body.push_str(&format!(
            "<li class=\"message-list-item\"><a class=\"activity-row\" href=\"{}\">{}</a>{}</li>",
            escape_html(&thread_action_url(&item.channel_id, &item.root.ts)),
            escape_html(&label),
            message_article(Some(&item.channel_id), &item.root, context),
        ));
    }
    body.push_str("</ol></main>");
    body.push_str(&emoji_picker_html(context));
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
    body.push_str(&emoji_picker_html(context));

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
    html_document_with_locales(
        title,
        body,
        script,
        &document_language(),
        configured_time_locale(),
    )
}

fn html_document_with_locales(
    title: &str,
    body: &str,
    script: Option<&str>,
    language: &str,
    time_locale: Option<&str>,
) -> String {
    let has_message_actions = body.contains("class=\"quick-actions\"");
    let has_author_actions =
        body.contains("class=\"author-actions\"") || body.contains("class=\"mention-actions\"");
    let needs_timestamp_localizer = body.contains("<time") || script.is_some();
    let scripts = [
        needs_timestamp_localizer.then_some(TIMESTAMP_LOCALIZATION_SCRIPT),
        script.filter(|script| !script.trim().is_empty()),
        has_message_actions.then_some(emoji_picker_script()),
        has_author_actions.then_some(author_actions_script()),
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
    let time_locale_attributes = time_locale
        .map(|locale| format!(" data-time-locale=\"{}\"", escape_html(locale)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="{}" dir="{}"{}>
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
  font: {MESSAGE_BASE_FONT_SIZE_CSS_PX}px/1.45 Cantarell, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
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

.profile-page {{
  box-sizing: border-box;
  max-inline-size: 720px;
  margin-inline: auto;
  padding-block: 24px;
  padding-inline: 20px;
}}

.profile-header {{
  display: flex;
  align-items: center;
  gap: 20px;
  margin-block: 28px;
}}

.profile-header h1 {{ margin: 0; }}
.profile-full-name {{ margin: 4px 0 0; color: var(--muted); }}
.profile-picture {{ inline-size: 128px; block-size: 128px; border-radius: 50%; object-fit: cover; }}
.profile-details {{ display: grid; gap: 0; margin: 0; }}
.profile-details > div {{ padding-block: 12px; border-block-end: 1px solid var(--line); }}
.profile-details dt {{ color: var(--muted); font-size: 12px; font-weight: 700; }}
.profile-details dd {{ margin: 3px 0 0; }}

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

.unread-separator {{
  display: flex;
  align-items: center;
  gap: 10px;
  margin-block: 4px;
  color: var(--accent);
  font-size: 12px;
  font-weight: 700;
}}

.unread-separator::before,
.unread-separator::after {{
  content: "";
  flex: 1;
  border-block-start: 1px solid currentColor;
}}

.unread-boundary-item:empty {{ display: none; }}

[data-message-region]:empty {{
  display: none;
}}

.message {{
  position: relative;
  display: grid;
  grid-template-columns: 36px minmax(0, 1fr);
  column-gap: 10px;
  row-gap: 6px;
  padding-block: 10px;
  padding-inline: 0;
  border-block-end: 1px solid var(--line);
}}

.message > :not(.message-avatar) {{
  grid-column: 2;
}}

.message-avatar {{
  grid-column: 1;
  grid-row: 1 / span 2;
  inline-size: 36px;
  block-size: 36px;
  border-radius: 8px;
  object-fit: cover;
}}

.message-avatar-fallback {{
  display: grid;
  place-items: center;
  background: var(--soft);
  color: var(--muted);
  font-size: 14px;
  font-weight: 700;
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

.author-actions,
.mention-actions {{
  position: relative;
  display: inline-block;
}}

.author-actions > summary {{
  cursor: pointer;
  list-style: none;
  padding-block: 1px;
  padding-inline: 4px;
  border-radius: 4px;
}}

.author-actions > summary::-webkit-details-marker {{ display: none; }}

.author-actions > summary::after {{
  content: "▾";
  margin-inline-start: 4px;
  color: var(--muted);
  font-size: 10px;
}}

.author-actions > summary:hover,
.author-actions[open] > summary {{
  background: var(--soft);
}}

.author-menu {{
  position: absolute;
  z-index: 10;
  display: grid;
  min-inline-size: 130px;
  margin-block-start: 4px;
  padding-block: 4px;
  padding-inline: 0;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--page);
  box-shadow: 0 6px 20px rgb(0 0 0 / 18%);
}}

.author-menu a {{
  padding-block: 7px;
  padding-inline: 12px;
}}

.author-menu a:hover {{
  background: var(--soft);
  text-decoration: none;
}}

.mention-actions > button {{
  border: 0;
  color: inherit;
  font: inherit;
  cursor: pointer;
}}

.mention-actions > .author-menu[hidden] {{
  display: none;
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

.user-status {{
  display: inline-flex;
  align-items: center;
  margin-inline-start: -4px;
  border-radius: 4px;
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

.channel-reference {{
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

.image-attachment,
.video-attachment {{
  display: grid;
  gap: 6px;
  max-inline-size: 520px;
  margin-block: 2px;
  margin-inline: 0;
}}

.image-attachment img,
.video-attachment img,
.video-attachment video {{
  display: block;
  inline-size: auto;
  max-inline-size: 100%;
  max-block-size: 420px;
  border-radius: 8px;
  background: var(--soft);
}}

.video-preview {{
  display: grid;
  inline-size: fit-content;
  max-inline-size: 100%;
}}

.video-preview > * {{
  grid-area: 1 / 1;
}}

.video-preview .image-placeholder {{
  min-inline-size: min(280px, 100%);
}}

.video-play {{
  display: grid;
  place-items: center;
  align-self: center;
  justify-self: center;
  inline-size: 52px;
  block-size: 52px;
  border-radius: 50%;
  background: rgba(0, 0, 0, 0.68);
  color: white;
  font-size: 24px;
  line-height: 1;
  z-index: 1;
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
  text-decoration: none;
}}

.reaction:hover {{
  text-decoration: none;
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

.emoji-picker {{
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

.emoji-picker::backdrop {{
  background: rgba(0, 0, 0, 0.28);
}}

.emoji-picker > header {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding-block: 14px 8px;
  padding-inline: 16px;
}}

.emoji-picker h2 {{
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
.emoji-choice:hover,
.emoji-choice[aria-selected="true"] {{
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

.emoji-categories[hidden] {{
  display: none;
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
  .quick-actions {{
    position: absolute;
    inset-block-start: 4px;
    inset-inline-end: 0;
    opacity: 0;
    pointer-events: none;
  }}

  .message:hover > .quick-actions,
  .message-part:hover > .quick-actions,
  .quick-actions:has(:focus-visible) {{
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
        time_locale_attributes,
        escape_html(title),
        body,
        script_tag
    )
}

fn timeline_scroll_script(channel_id: &str, behavior: TimelineScrollBehavior) -> Option<String> {
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
  const threshold = 96;

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

  function rememberViewport() {{
    rememberPosition();
    rememberAnchor();
  }}

  function restoreAnchor() {{
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
    rememberViewport();
  }}

  function scrollToBottom() {{
    const scrollRoot = root();
    scrollRoot.scrollTop = scrollRoot.scrollHeight;
    rememberViewport();
  }}

  const shouldStick = wasAtBottom();
  function applyScroll() {{
    if (shouldStick) {{
      scrollToBottom();
    }} else {{
      restoreAnchor();
    }}
  }}

  document.addEventListener("click", function (event) {{
    const target = event.target && event.target.closest
      ? event.target.closest("a[href^='conduit://load-older']")
      : null;
    if (target) {{
      rememberViewport();
    }}
  }}, true);

  if (mode === "preserve-prepend") {{
    window.addEventListener("scroll", rememberViewport, {{ passive: true }});
    window.addEventListener("load", restoreAnchor, {{ once: true }});
    requestAnimationFrame(restoreAnchor);
    requestAnimationFrame(function () {{
      requestAnimationFrame(restoreAnchor);
    }});
    return;
  }}

  window.addEventListener("scroll", rememberViewport, {{ passive: true }});
  window.addEventListener("load", applyScroll, {{ once: true }});
  requestAnimationFrame(applyScroll);
  requestAnimationFrame(function () {{
    requestAnimationFrame(applyScroll);
  }});
}})();"#
    ))
}

fn timeline_scroll_identity(channel_id: &str, thread_ts: Option<&str>) -> String {
    match thread_ts {
        Some(thread_ts) => format!("thread:{channel_id}:{thread_ts}"),
        None => format!("channel:{channel_id}"),
    }
}

fn timeline_dom_runtime_script() -> &'static str {
    include_str!("timeline_dom_runtime.js")
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
  if (!("IntersectionObserver" in window)) return;
  const sentinel = document.getElementById("timeline-read-sentinel");
  if (sentinel) {{
    const sentinelObserver = new IntersectionObserver(function (entries) {{
      if (!entries.some(function (entry) {{ return entry.isIntersecting; }})) return;
      sentinelObserver.disconnect();
      window.location.href = {url};
    }}, {{ threshold: 1.0 }});
    sentinelObserver.observe(sentinel);
    return;
  }}
  let lastSent = "";
  let timer = 0;
  const visible = new Set();
  function timestampAfter(left, right) {{ return left.localeCompare(right) > 0; }}
  function schedule() {{
    window.clearTimeout(timer);
    timer = window.setTimeout(function () {{
      const newest = Array.from(visible).sort().pop();
      if (!newest || !timestampAfter(newest, lastSent)) return;
      lastSent = newest;
      const ordered = Array.from(document.querySelectorAll("[data-message-ts]"));
      const currentIndex = ordered.findIndex(function (message) {{ return message.dataset.messageTs === newest; }});
      const next = currentIndex >= 0 ? ordered[currentIndex + 1] : null;
      const separator = document.querySelector(".unread-separator");
      if (separator && next) next.before(separator);
      else if (separator) separator.remove();
      const target = new URL({url});
      target.searchParams.set("ts", newest);
      window.location.href = target.toString();
    }}, 500);
  }}
  const observer = new IntersectionObserver(function (entries) {{
    entries.forEach(function (entry) {{
      const ts = entry.target.dataset.messageTs;
      if (!ts) return;
      if (entry.isIntersecting) visible.add(ts); else visible.delete(ts);
    }});
    schedule();
  }}, {{ threshold: 0.01 }});
  function observeUnreadMessages() {{
    const boundary = document.querySelector(".unread-separator");
    if (!boundary) return;
    let afterBoundary = false;
    document.querySelectorAll(".unread-separator, [data-message-ts]").forEach(function (node) {{
      if (node.classList.contains("unread-separator")) {{ afterBoundary = true; return; }}
      if (afterBoundary && !node.dataset.readObserved) {{
        node.dataset.readObserved = "true";
        observer.observe(node);
      }}
    }});
  }}
  observeUnreadMessages();
  new MutationObserver(observeUnreadMessages).observe(document.querySelector(".message-list"), {{ childList: true, subtree: true }});
}})();"#
    )
}

fn message_article(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let author = author_label(message, context);
    let status = author_status_html(message, context);
    let avatar = message_avatar_html(message, &author, context);
    let mut article = format!(
        "<article class=\"message\"{}{}>{}<header class=\"message-header\">{}{}</header><div data-message-region=\"body\"><div class=\"body\" dir=\"auto\">{}</div></div>",
        message_target_attributes(Some(&message.ts)),
        message_author_attribute(message),
        avatar,
        author_identity_html(message, &author, &status, context),
        metadata_html(message),
        message_body_html(message, context)
    );

    article.push_str("<div data-message-region=\"attachments\">");
    article.push_str(&attachments_html(channel_id, message, context));
    article.push_str("</div><div data-message-region=\"responses\">");
    article.push_str(&message_responses_html(channel_id, message, context));
    article.push_str("</div>");
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
    let action_url = item
        .thread_ts
        .as_deref()
        .map(|thread_ts| thread_action_url(&item.channel_id, thread_ts))
        .unwrap_or_else(|| unreads_open_action_url(&item.channel_id));
    format!(
        concat!(
            "<li><a class=\"activity-row\" href=\"{}\">",
            "<span class=\"activity-title\" dir=\"auto\">{}</span>",
            "<span class=\"activity-badge\">{}</span>",
            "<span class=\"activity-meta\">{}</span>",
            "</a></li>"
        ),
        escape_html(&action_url),
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
    let status = author_status_html(first_message, context);
    let avatar = message_avatar_html(first_message, &author, context);
    let mut article = format!(
        "<article class=\"message message-group\"{}>{}<header class=\"message-header\">{}{}</header><div class=\"message-stack\">",
        message_author_attribute(first_message),
        avatar,
        author_identity_html(first_message, &author, &status, context),
        metadata_html(first_message)
    );

    for message in messages {
        article.push_str(&message_part_html(channel_id, message, context));
    }

    article.push_str("</div></article>");
    article
}

fn message_avatar_html(
    message: &SlackMessage,
    author: &str,
    context: &MessageHtmlContext,
) -> String {
    let source = message
        .user
        .as_ref()
        .and_then(|user_id| context.user_avatar_urls.get(user_id))
        .and_then(|url| context.image_assets.get(url))
        .filter(|source| source.starts_with("data:image/"));
    if let Some(source) = source {
        return format!(
            "<img class=\"message-avatar\" src=\"{}\" alt=\"\" aria-hidden=\"true\">",
            escape_html(source)
        );
    }

    let initial = author
        .trim()
        .chars()
        .next()
        .map(|character| character.to_uppercase().collect::<String>())
        .filter(|initial| !initial.is_empty())
        .unwrap_or_else(|| "?".to_string());
    format!(
        "<span class=\"message-avatar message-avatar-fallback\" aria-hidden=\"true\">{}</span>",
        escape_html(&initial)
    )
}

fn author_identity_html(
    message: &SlackMessage,
    author: &str,
    status: &str,
    context: &MessageHtmlContext,
) -> String {
    let label = escape_html(author);
    let Some(user_id) = message.user.as_deref().filter(|id| !id.is_empty()) else {
        return format!("<span class=\"author author-label\" dir=\"auto\">{label}</span>{status}");
    };
    let tooltip = context
        .user_full_names
        .get(user_id)
        .map(String::as_str)
        .unwrap_or(author);
    format!(
        "<details class=\"author-actions\"><summary class=\"author author-label\" dir=\"auto\" title=\"{}\">{label}</summary><nav class=\"author-menu\" aria-label=\"{}\"><a href=\"{}\">{}</a><a href=\"{}\">{}</a></nav></details>{status}",
        escape_html(tooltip),
        escape_html(&gettext("Person actions")),
        escape_html(&user_message_action_url(user_id)),
        escape_html(&gettext("Message")),
        escape_html(&user_profile_action_url(user_id)),
        escape_html(&gettext("Profile")),
    )
}

fn mention_actions_html(user_id: &str, name: &str, tooltip: &str) -> String {
    let label = format!("@{name}");
    format!(
        "<span class=\"mention-actions\"><button type=\"button\" class=\"mention\" title=\"{}\" data-mention-user-id=\"{}\" aria-haspopup=\"menu\" aria-expanded=\"false\">{}</button><span class=\"author-menu\" role=\"menu\" aria-label=\"{}\" hidden><a href=\"{}\">{}</a><a href=\"{}\">{}</a></span></span>",
        escape_html(tooltip),
        escape_html(user_id),
        escape_html(&label),
        escape_html(&gettext("Person actions")),
        escape_html(&user_message_action_url(user_id)),
        escape_html(&gettext("Message")),
        escape_html(&user_profile_action_url(user_id)),
        escape_html(&gettext("Profile")),
    )
}

fn message_part_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let mut part = format!(
        "<div class=\"message-part\"{}{}><div data-message-region=\"body\"><div class=\"body\" dir=\"auto\">{}</div></div>",
        message_target_attributes(Some(&message.ts)),
        message_author_attribute(message),
        message_body_html(message, context)
    );

    part.push_str("<div data-message-region=\"attachments\">");
    part.push_str(&attachments_html(channel_id, message, context));
    part.push_str("</div><div data-message-region=\"responses\">");
    part.push_str(&message_responses_html(channel_id, message, context));
    part.push_str("</div>");
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

fn message_author_attribute(message: &SlackMessage) -> String {
    message
        .user
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
        .map(|user_id| format!(" data-author-user-id=\"{}\"", escape_html(user_id)))
        .unwrap_or_default()
}

fn message_groups<'a>(
    messages: &'a [SlackMessage],
    first_unread_ts: Option<&str>,
) -> Vec<Vec<&'a SlackMessage>> {
    let ordered = messages.iter().rev().collect::<Vec<_>>();
    let mut groups: Vec<Vec<&SlackMessage>> = Vec::new();

    for message in ordered {
        if let Some(group) = groups.last_mut() {
            if first_unread_ts != Some(message.ts.as_str())
                && group
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

fn unread_separator_html() -> String {
    format!(
        "<li class=\"unread-boundary-item\"><div class=\"unread-separator\" role=\"separator\" aria-label=\"{}\"><span>{}</span></div></li>",
        escape_html(&gettext("Unread messages")),
        escape_html(&gettext("New"))
    )
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
        .and_then(|channel| {
            channel
                .id
                .as_deref()
                .and_then(|id| context.conversation_titles.get(id).cloned())
                .or_else(|| channel.name.as_deref().map(|name| format!("#{name}")))
        })
        .unwrap_or_else(|| "Slack".to_string());
    let author = result
        .user
        .as_deref()
        .and_then(|user_id| context.user_names.get(user_id).map(String::as_str))
        .or(result.username.as_deref())
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
    let now = gtk::glib::DateTime::now_local().ok()?;
    localized_timestamp_parts_at(&datetime, &now)
}

fn localized_timestamp_parts_at(
    datetime: &gtk::glib::DateTime,
    now: &gtk::glib::DateTime,
) -> Option<(String, String, String)> {
    let machine = datetime.format_iso8601().ok()?.to_string();
    let localized = datetime.format("%c").ok()?.to_string();
    let timezone = datetime.format("%Z").ok()?.to_string();
    let full = full_timestamp_with_timezone(&localized, &timezone);
    let short = compact_timestamp_text(datetime, now)?;
    Some((machine, full, short))
}

fn full_timestamp_with_timezone(localized: &str, timezone: &str) -> String {
    let timezone = timezone.trim();
    let already_present = localized.split_whitespace().any(|part| {
        part.trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '+' && character != '-'
        })
        .eq_ignore_ascii_case(timezone)
    });
    if timezone.is_empty() || already_present {
        localized.to_string()
    } else {
        format!("{localized} {timezone}")
    }
}

fn compact_timestamp_text(
    datetime: &gtk::glib::DateTime,
    now: &gtk::glib::DateTime,
) -> Option<String> {
    let time = datetime.format(&gettext("%H:%M")).ok()?.to_string();
    let days_old = local_calendar_day(datetime) - local_calendar_day(now);
    let days_old = -days_old;

    let day = match days_old {
        0 => return Some(time),
        1 => gettext("Yesterday"),
        2..=5 => datetime.format("%A").ok()?.to_string(),
        _ => {
            let include_year = days_old >= 183 && datetime.year() != now.year();
            let format = if include_year {
                gettext("%b %e, %Y")
            } else {
                gettext("%b %e")
            };
            datetime
                .format(&format)
                .ok()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        }
    };

    Some(
        gettext("{day}, {time}")
            .replace("{day}", &day)
            .replace("{time}", &time),
    )
}

fn local_calendar_day(datetime: &gtk::glib::DateTime) -> i64 {
    // Convert a civil date to a monotonic day number. Time and UTC offset are
    // intentionally ignored: relative labels follow the user's local dates.
    let mut year = i64::from(datetime.year());
    let month = i64::from(datetime.month());
    let day = i64::from(datetime.day_of_month());
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year
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

fn author_status_html(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    let Some(user_id) = message.user.as_deref() else {
        return String::new();
    };
    let Some(status) = context.user_statuses.get(user_id) else {
        return String::new();
    };
    if !status.active_at(current_unix_seconds()) {
        return String::new();
    }
    user_status_html(status, &context.custom_emojis)
}

fn user_status_html(status: &SlackUserStatus, custom_emojis: &HashMap<String, String>) -> String {
    let accessible = status.accessible_text();
    let glyph = EmojiCatalog::new(custom_emojis)
        .resolve(status.emoji_name())
        .map(|value| emoji_value_html(&value, false))
        .unwrap_or_else(|| "●".to_string());
    format!(
        "<span class=\"user-status\" tabindex=\"0\" role=\"img\" title=\"{}\" aria-label=\"{}\">{glyph}</span>",
        escape_html(&accessible),
        escape_html(&format!("Status: {accessible}")),
    )
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
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
        String::new()
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
                if kind == "video" {
                    if let Some(poster_url) = file.video_preview_url() {
                        return video_figure_html(poster_url, &viewer_url, label, context);
                    }
                    return attachment_chip_html(label, Some(&viewer_url));
                }
                return attachment_chip_html(label, Some(&viewer_url));
            }

            let download_url = file
                .download_url()
                .filter(|url| is_http_url(url))
                .map(|url| {
                    let filename = file
                        .name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(label);
                    attachment_action_url(url, filename)
                });
            attachment_chip_html(label, download_url.as_deref())
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

pub fn attachment_action_url(url: &str, name: &str) -> String {
    format!(
        "conduit://attachment?url={}&name={}",
        encode_query(url),
        encode_query(name),
    )
}

fn attachment_chip_html(label: &str, link: Option<&str>) -> String {
    let label = escape_html(&gettext("Attachment: {name}").replace("{name}", label));
    if let Some(link) = link.filter(|link| {
        is_http_url(link)
            || link.starts_with("conduit://media?")
            || link.starts_with("conduit://attachment?")
    }) {
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
    let image = preview_image_html(
        asset_key,
        alt,
        &gettext("Loading image preview"),
        &gettext("Image preview unavailable"),
        context,
    );

    let caption = attachment_caption_html(caption);
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

fn video_figure_html(
    poster_key: &str,
    viewer_url: &str,
    label: &str,
    context: &MessageHtmlContext,
) -> String {
    let alt = gettext("Video preview: {name}").replace("{name}", label);
    let poster = if context
        .image_assets
        .get(poster_key)
        .is_some_and(|asset| asset.starts_with("data:video/"))
    {
        let src = context.image_assets.get(poster_key).unwrap();
        format!(
            "<video preload=\"metadata\" muted playsinline src=\"{}\" aria-label=\"{}\" data-image-key=\"{}\" data-image-alt=\"{}\"></video>",
            escape_html(src),
            escape_html(&alt),
            escape_html(poster_key),
            escape_html(&alt),
        )
    } else {
        preview_image_html(
            poster_key,
            &alt,
            &gettext("Loading video preview"),
            &gettext("Video preview unavailable"),
            context,
        )
    };
    let play = "<span class=\"video-play\" aria-hidden=\"true\">▶</span>";
    let caption = attachment_caption_html(Some(label));
    let aria_label = gettext("Play video: {name}").replace("{name}", label);

    format!(
        "<a class=\"video-link\" href=\"{}\" aria-label=\"{}\" rel=\"noreferrer noopener\"><figure class=\"video-attachment\"><div class=\"video-preview\">{poster}{play}</div>{caption}</figure></a>",
        escape_html(viewer_url),
        escape_html(&aria_label),
    )
}

fn preview_image_html(
    asset_key: &str,
    alt: &str,
    loading_label: &str,
    unavailable_label: &str,
    context: &MessageHtmlContext,
) -> String {
    let patch_attributes = format!(
        " data-image-key=\"{}\" data-image-alt=\"{}\" data-image-unavailable=\"{}\"",
        escape_html(asset_key),
        escape_html(alt),
        escape_html(unavailable_label),
    );
    if let Some(src) = context.image_assets.get(asset_key) {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=loaded key={}", debug::url_for_log(asset_key)),
            );
        }
        format!(
            "<img loading=\"lazy\" decoding=\"async\" src=\"{}\" alt=\"{}\"{}>",
            escape_html(src),
            escape_html(alt),
            patch_attributes,
        )
    } else if context.failed_image_urls.contains(asset_key) {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=failed key={}", debug::url_for_log(asset_key)),
            );
        }
        format!(
            "<div class=\"image-placeholder\"{}>{}</div>",
            patch_attributes,
            escape_html(unavailable_label),
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
            "<img loading=\"lazy\" decoding=\"async\" src=\"{}\" alt=\"{}\"{}>",
            escape_html(asset_key),
            escape_html(alt),
            patch_attributes,
        )
    } else {
        if debug::enabled() {
            debug::log(
                "render",
                &format!("image state=pending key={}", debug::url_for_log(asset_key)),
            );
        }
        format!(
            "<div class=\"image-placeholder\"{}>{}</div>",
            patch_attributes,
            escape_html(loading_label),
        )
    }
}

fn attachment_caption_html(caption: Option<&str>) -> String {
    let caption = caption
        .filter(|caption| !caption.trim().is_empty())
        .map(|caption| {
            format!(
                "<figcaption class=\"image-caption\" dir=\"auto\">{}</figcaption>",
                escape_html(caption)
            )
        })
        .unwrap_or_default();
    caption
}

fn message_responses_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
    let mut responses = reactions_html(channel_id, message, context);
    responses.push_str(&thread_response_html(channel_id, message, context));

    if responses.is_empty() {
        String::new()
    } else {
        format!("<div class=\"reactions\">{responses}</div>")
    }
}

fn reactions_html(
    channel_id: Option<&str>,
    message: &SlackMessage,
    context: &MessageHtmlContext,
) -> String {
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
                gettext("{names}: {reaction}")
                    .replace("{names}", &participants.join(", "))
                    .replace("{reaction}", &reaction_tooltip_text(name, context))
            };
            let content = format!("{} {count}", reaction_label(name, context));
            Some(if let Some(channel_id) = channel_id.filter(|_| !message.ts.is_empty()) {
                format!(
                    "<a class=\"reaction{}\" href=\"{}\" title=\"{}\" aria-label=\"{}\">{}</a>",
                    active_class,
                    escape_html(&reaction_action_url(
                        channel_id,
                        message,
                        name,
                        !active,
                        action_thread_ts(message, context),
                    )),
                    escape_html(&tooltip),
                    escape_html(&tooltip),
                    content,
                )
            } else {
                format!(
                    "<span class=\"reaction{}\" tabindex=\"0\" title=\"{}\" aria-label=\"{}\">{}</span>",
                    active_class,
                    escape_html(&tooltip),
                    escape_html(&tooltip),
                    content,
                )
            })
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
        "<button type=\"button\" class=\"action-button\" data-open-emoji-picker data-reaction-template=\"{}\" title=\"{}\" aria-label=\"{}\">☺<span aria-hidden=\"true\">+</span></button>",
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

pub fn user_message_action_url(user_id: &str) -> String {
    format!("conduit://user-message?user={}", encode_query(user_id))
}

pub fn channel_action_url(channel_id: &str) -> String {
    format!("conduit://channel?channel={}", encode_query(channel_id))
}

pub fn user_profile_action_url(user_id: &str) -> String {
    format!("conduit://user-profile?user={}", encode_query(user_id))
}

pub fn mark_thread_read_action_url(channel_id: &str, thread_ts: &str, ts: &str) -> String {
    format!(
        "conduit://mark-read?channel={}&thread_ts={}&ts={}",
        encode_query(channel_id),
        encode_query(thread_ts),
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

fn reaction_tooltip_text(name: &str, context: &MessageHtmlContext) -> String {
    match EmojiCatalog::new(&context.custom_emojis).resolve(name) {
        Some(EmojiValue::Unicode(value)) => value.to_string(),
        Some(EmojiValue::CustomImage(_)) | None => format!(":{name}:"),
    }
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
        if let Some((_, consumed)) = decode_html_entity_prefix(rest) {
            output.push_str(&escape_html(&rest[..consumed]));
            rest = &rest[consumed..];
            continue;
        }
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

        if let Some((html, consumed)) = render_bare_channel_reference(rest, context) {
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
        let tooltip = context
            .user_full_names
            .get(user_id)
            .map(String::as_str)
            .unwrap_or(&name);
        mention_actions_html(user_id, &name, tooltip)
    } else if raw.starts_with("!subteam^") {
        user_group_mention_html(raw, context)
    } else if let Some(channel) = raw.strip_prefix('#') {
        let (channel_id, fallback) = channel
            .split_once('|')
            .map_or((channel, None), |(channel_id, label)| {
                (channel_id, Some(label))
            });
        let display = context
            .conversation_titles
            .get(channel_id)
            .cloned()
            .unwrap_or_else(|| format!("#{}", fallback.unwrap_or(channel_id)));
        channel_reference_html(channel_id, &display)
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

fn render_bare_channel_reference(
    text: &str,
    context: &MessageHtmlContext,
) -> Option<(String, usize)> {
    let candidate = text.strip_prefix('#')?;
    let id_length = candidate
        .bytes()
        .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        .count();
    if id_length == 0 {
        return None;
    }
    let channel_id = &candidate[..id_length];
    let title = context.conversation_titles.get(channel_id)?;
    Some((channel_reference_html(channel_id, title), id_length + 1))
}

fn channel_reference_html(channel_id: &str, label: &str) -> String {
    format!(
        "<a class=\"channel-reference\" href=\"{}\">{}</a>",
        escape_html(&channel_action_url(channel_id)),
        escape_html(label)
    )
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
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((character, consumed)) = decode_html_entity_prefix(rest) {
            push_escaped_html_character(&mut escaped, character);
            rest = &rest[consumed..];
            continue;
        }

        let character = rest.chars().next().expect("non-empty text has a character");
        push_escaped_html_character(&mut escaped, character);
        rest = &rest[character.len_utf8()..];
    }
    escaped
}

fn decode_html_entity_prefix(text: &str) -> Option<(char, usize)> {
    if !text.starts_with('&') {
        return None;
    }
    let end = text
        .as_bytes()
        .iter()
        .take(16)
        .position(|byte| *byte == b';')?;
    let entity = &text[1..end];
    let character = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00a0}',
        entity if entity.starts_with("#x") || entity.starts_with("#X") => {
            char::from_u32(u32::from_str_radix(&entity[2..], 16).ok()?)?
        }
        entity if entity.starts_with('#') => char::from_u32(entity[1..].parse().ok()?)?,
        _ => return None,
    };
    Some((character, end + 1))
}

fn push_escaped_html_character(output: &mut String, character: char) {
    match character {
        '&' => output.push_str("&amp;"),
        '<' => output.push_str("&lt;"),
        '>' => output.push_str("&gt;"),
        '"' => output.push_str("&quot;"),
        '\'' => output.push_str("&#39;"),
        _ => output.push(character),
    }
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
            normalize_language_tag("ks_IN@devanagari"),
            Some("ks-Deva-IN".into())
        );
        assert_eq!(
            normalize_language_tag("tt_RU@iqtelif"),
            Some("tt-Latn-RU".into())
        );
        assert_eq!(
            normalize_language_tag("ca_ES@valencia"),
            Some("ca-ES-valencia".into())
        );
        assert_eq!(
            normalize_language_tag("sr_RS@ijekavianlatin"),
            Some("sr-Latn-RS-ijekavsk".into())
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
        assert_eq!(normalize_language_tag("c.UTF-8"), None);
        assert_eq!(normalize_language_tag("POSIX"), None);
        assert_eq!(normalize_language_tag("posix.utf8"), None);
        assert_eq!(normalize_language_tag("en\"><script>"), None);
        assert_eq!(
            normalize_time_format_locale(Some(b"nl_NL.UTF-8")),
            Some("nl-NL".into())
        );
        assert_eq!(normalize_time_format_locale(Some(&[0xff, 0xfe])), None);
        assert_eq!(
            preferred_time_locale([None, Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
            Some("nl-NL".into())
        );
        assert_eq!(
            preferred_time_locale([Some(""), Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
            Some("nl-NL".into())
        );
        assert_eq!(
            preferred_time_locale([Some("C.UTF-8"), Some("nl_NL.UTF-8")]),
            None
        );

        let html = html_document_with_locales(
            "Messages",
            "<main></main>",
            None,
            "en\"><script>alert(1)</script>",
            Some("nl-NL\"><script>alert(2)</script>"),
        );
        assert!(html.contains(
            "<html lang=\"en&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\" dir=\"ltr\" data-time-locale=\"nl-NL&quot;&gt;&lt;script&gt;alert(2)&lt;/script&gt;\">"
        ));
        assert!(!html.contains("<html lang=\"en\"><script>"));
        assert!(!html.contains("<script>alert(2)</script>"));
    }

    #[test]
    fn document_root_direction_follows_the_normalized_primary_language() {
        for language in ["ar", "ar-EG", "he-IL"] {
            let html = html_document_with_locales("Title", "<main></main>", None, language, None);
            assert!(
                html.contains(&format!("<html lang=\"{language}\" dir=\"rtl\">")),
                "{language}"
            );
        }

        let html = html_document_with_locales("Title", "<main></main>", None, "en-GB", None);
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
    fn webkit_documents_decode_entities_once_before_safe_rendering() {
        assert_eq!(
            escape_html("&gt; &lt; &amp; &quot; &apos; &#62; &#x1F642;"),
            "&gt; &lt; &amp; &quot; &#39; &gt; 🙂"
        );
        assert_eq!(
            escape_html("&amp;lt;script&amp;gt;"),
            "&amp;lt;script&amp;gt;"
        );

        let placeholder = placeholder_document("A &gt; B", "C &lt; D &amp; E");
        assert!(placeholder.contains("<title>A &gt; B</title>"));
        assert!(placeholder.contains("C &lt; D &amp; E"));
        assert!(!placeholder.contains("&amp;gt;"));

        let conversation = conversation_document(
            "C123",
            &[message("A &gt; B &amp; &lt;script&gt;")],
            &MessageHtmlContext::default(),
        );
        assert!(conversation.contains("A &gt; B &amp; &lt;script&gt;"));
        assert!(!conversation.contains("A &amp;gt; B"));

        let search = search_results_document(
            &[SearchMatch {
                text: Some("Result &gt; threshold".into()),
                ..Default::default()
            }],
            &MessageHtmlContext::default(),
        );
        assert!(search.contains("Result &gt; threshold"));
        assert!(!search.contains("Result &amp;gt; threshold"));

        let profile = user_profile_document(
            &SlackUser {
                real_name: Some("Ada &gt; Grace".into()),
                ..Default::default()
            },
            &MessageHtmlContext::default(),
        );
        assert!(profile.contains("Ada &gt; Grace"));
        assert!(!profile.contains("Ada &amp;gt; Grace"));

        let files = files_document(&[SlackFile {
            title: Some("Report &gt; draft".into()),
            ..Default::default()
        }]);
        assert!(files.contains("Report &gt; draft"));
        assert!(!files.contains("Report &amp;gt; draft"));
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
        assert!(fine_pointer_css.contains("position: absolute"));
        assert!(fine_pointer_css.contains("inset-block-start: 4px"));
        assert!(fine_pointer_css.contains("inset-inline-end: 0"));
        assert!(fine_pointer_css.contains(".message:hover > .quick-actions"));
        assert!(fine_pointer_css.contains(".message-part:hover > .quick-actions"));
        assert!(fine_pointer_css.contains(".quick-actions:has(:focus-visible)"));
        assert!(!fine_pointer_css.contains(".message:focus-within"));
        assert!(!fine_pointer_css.contains(".message-part:focus-within"));
        assert!(!fine_pointer_css.contains(".quick-actions:focus-within"));
        assert!(!fine_pointer_css.contains("grid-template-columns"));
        assert!(!fine_pointer_css.contains("grid-column"));
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
    fn message_groups_render_one_cached_avatar_with_an_initials_fallback() {
        let avatar_url = "https://avatars.slack-edge.com/ada.png";
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U123".to_string(), "Ada".to_string())]),
            user_avatar_urls: HashMap::from([("U123".to_string(), avatar_url.to_string())]),
            image_assets: HashMap::from([(
                avatar_url.to_string(),
                "data:image/png;base64,YXZhdGFy".to_string(),
            )]),
            ..Default::default()
        };
        let messages = [
            message_at("U123", "second", "1710000001.000100"),
            message_at("U123", "first", "1710000000.000100"),
        ];

        let html = conversation_document("C123", &messages, &context);
        assert_eq!(html.matches("class=\"message-avatar\"").count(), 1);
        assert!(html.contains("src=\"data:image/png;base64,YXZhdGFy\""));
        assert!(html.contains("alt=\"\" aria-hidden=\"true\""));

        let fallback = conversation_document(
            "C123",
            &[message("hello")],
            &MessageHtmlContext {
                user_names: HashMap::from([("U123".to_string(), "Ada".to_string())]),
                ..Default::default()
            },
        );
        assert!(fallback.contains(
            "class=\"message-avatar message-avatar-fallback\" aria-hidden=\"true\">A</span>"
        ));
        assert!(fallback.contains("grid-template-columns: 36px minmax(0, 1fr)"));
        assert!(fallback.contains("grid-row: 1 / span 2"));
    }

    #[test]
    fn author_status_is_accessible_in_direct_group_and_channel_messages() {
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U123".to_string(), "Ada".to_string())]),
            user_full_names: HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]),
            user_statuses: HashMap::from([(
                "U123".to_string(),
                SlackUserStatus {
                    text: "Heads <down>".to_string(),
                    emoji: ":brain:".to_string(),
                    expiration: i64::MAX,
                },
            )]),
            ..Default::default()
        };

        for conversation_id in ["D123", "G123", "C123"] {
            let html = conversation_document(conversation_id, &[message("hello")], &context);

            assert!(html.contains(
                "class=\"author author-label\" dir=\"auto\" title=\"Ada Lovelace\">Ada</summary>"
            ));
            assert!(html.contains("<span class=\"user-status\""));
            assert!(html.contains("title=\"Heads &lt;down&gt;\""));
            assert!(html.contains("aria-label=\"Status: Heads &lt;down&gt;\""));
            assert!(html.contains("tabindex=\"0\""));
        }
    }

    #[test]
    fn resolves_mentions_channels_and_slack_links() {
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U123".to_string(), "Ada".to_string())]),
            user_full_names: HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]),
            conversation_titles: HashMap::from([
                ("C999".to_string(), "#general-renamed".to_string()),
                (
                    "C0BHX4E4TRT".to_string(),
                    "#warroom-servinform-data-breach".to_string(),
                ),
            ]),
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

        assert!(html.contains(
            "<button type=\"button\" class=\"mention\" title=\"Ada Lovelace\" data-mention-user-id=\"U123\" aria-haspopup=\"menu\" aria-expanded=\"false\">@Ada</button>"
        ));
        assert!(html.contains("conduit://user-message?user=U123"));
        assert!(html.contains("conduit://user-profile?user=U123"));
        assert!(html.contains("href=\"conduit://channel?channel=C999\">#general-renamed</a>"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains(">docs</a>"));

        assert_eq!(
            mrkdwn_to_html("FYI we are here\n#C0BHX4E4TRT", &context),
            "FYI we are here<br><a class=\"channel-reference\" href=\"conduit://channel?channel=C0BHX4E4TRT\">#warroom-servinform-data-breach</a>"
        );
        assert_eq!(
            mrkdwn_to_html("`#C0BHX4E4TRT` ```#C0BHX4E4TRT```", &context),
            "<code>#C0BHX4E4TRT</code> <pre><code>#C0BHX4E4TRT</code></pre>"
        );
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
        assert!(html.contains("categories.hidden = terms.length > 0"));
        assert!(html.contains(".emoji-categories[hidden]"));
        assert!(html.contains("Math.min(90, 50 + termLength * 10)"));
        assert!(html.contains("Math.min(Math.floor((score || 0) / 5), 19)"));
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
    fn timestamp_tooltip_includes_the_timezone_once() {
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16 CEST", "CEST"),
            "do 02 jul 2026 10:42:16 CEST"
        );
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16", "CEST"),
            "do 02 jul 2026 10:42:16 CEST"
        );
        assert_eq!(
            full_timestamp_with_timezone("do 02 jul 2026 10:42:16", ""),
            "do 02 jul 2026 10:42:16"
        );
    }

    #[test]
    fn timestamp_documents_install_the_webkit_intl_localizer() {
        let body = r#"<time class="metadata" datetime="2026-07-10T13:00:00+02:00" title="fallback title">jul 10, 13:00</time>"#;
        let html = html_document_with_locales("Messages", body, None, "en", Some("nl-NL"));

        assert!(html.contains("data-time-locale=\"nl-NL\""));
        assert!(html.contains("new Intl.DateTimeFormat"));
        assert!(html.contains("new Intl.RelativeTimeFormat"));
        assert!(html.contains("Intl.DateTimeFormat.supportedLocalesOf"));
        assert!(html.contains("window.conduitLocalizeTimestamps = localizeTimestamps"));
        assert!(html.contains("localizeTimestamps(document)"));
        assert_eq!(
            html.matches("window.conduitLocalizeTimestamps = localizeTimestamps")
                .count(),
            1
        );
        assert!(html.contains(body));

        let without_timestamp =
            html_document_with_locales("Messages", "<main></main>", None, "en", Some("nl-NL"));
        assert!(!without_timestamp.contains("new Intl.DateTimeFormat"));

        let c_locale = html_document_with_locales("Messages", body, None, "en", None);
        assert!(!c_locale.contains("data-time-locale"));
        assert!(c_locale.contains("if (!locale) return;"));
        assert!(c_locale.contains(body));

        let patchable_without_timestamp = html_document_with_locales(
            "Messages",
            "<main></main>",
            Some("window.conduitApplyTimelinePatch = function () {};"),
            "en",
            Some("nl-NL"),
        );
        assert!(patchable_without_timestamp.contains("new Intl.DateTimeFormat"));
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
    fn compact_timestamp_uses_relative_days_then_dates_and_years() {
        let timezone = gtk::glib::TimeZone::local();
        let now = gtk::glib::DateTime::new(&timezone, 2026, 7, 15, 12, 0, 0.0).unwrap();
        let at = |year, month, day| {
            gtk::glib::DateTime::new(&timezone, year, month, day, 9, 30, 0.0).unwrap()
        };

        let today = at(2026, 7, 15);
        assert_eq!(
            compact_timestamp_text(&today, &now).as_deref(),
            today.format(&gettext("%H:%M")).ok().as_deref()
        );

        let yesterday = compact_timestamp_text(&at(2026, 7, 14), &now).unwrap();
        assert!(yesterday.contains(&gettext("Yesterday")));

        let five_days_ago = at(2026, 7, 10);
        let weekday = five_days_ago.format("%A").unwrap().to_string();
        assert!(compact_timestamp_text(&five_days_ago, &now)
            .unwrap()
            .contains(&weekday));

        let six_days_ago = at(2026, 7, 9);
        let date_without_year = six_days_ago
            .format(&gettext("%b %e"))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let six_days_label = compact_timestamp_text(&six_days_ago, &now).unwrap();
        assert!(six_days_label.contains(&date_without_year));
        assert!(!six_days_label.contains("2026"));

        let recent_previous_year_now = at(2026, 1, 10);
        let recent_previous_year = at(2025, 12, 31);
        assert!(
            !compact_timestamp_text(&recent_previous_year, &recent_previous_year_now)
                .unwrap()
                .contains("2025")
        );

        let old_previous_year = at(2025, 12, 31);
        assert!(compact_timestamp_text(&old_previous_year, &now)
            .unwrap()
            .contains("2025"));
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
        assert!(html.contains("conduit:timeline-anchor:channel:C123"));
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
    fn empty_attachment_messages_do_not_render_a_text_placeholder() {
        let image_url = "https://files.slack.com/files-pri/T123-F123/image.png";
        let mut message = message("");
        message.files = Some(vec![SlackFile {
            title: Some("Screenshot".to_string()),
            mimetype: Some("image/png".to_string()),
            thumb_480: Some(image_url.to_string()),
            ..Default::default()
        }]);
        let context = MessageHtmlContext {
            image_assets: HashMap::from([(
                image_url.to_string(),
                "data:image/png;base64,image".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(!html.contains("No message text"));
        assert!(html.contains("src=\"data:image/png;base64,image\""));
        assert!(html.contains("Screenshot"));
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
        message.reactions = Some(vec![
            SlackReaction {
                name: Some("thumbsup".to_string()),
                count: Some(3),
                users: Some(vec![
                    "U999".to_string(),
                    "U456".to_string(),
                    "U123".to_string(),
                ]),
            },
            SlackReaction {
                name: Some("eyes".to_string()),
                count: Some(1),
                users: Some(vec!["U456".to_string()]),
            },
        ]);
        let context = MessageHtmlContext {
            current_user_id: Some("U999".to_string()),
            user_names: HashMap::from([
                ("U999".to_string(), "Ada Lovelace".to_string()),
                ("U456".to_string(), "Grace Hopper".to_string()),
                ("U123".to_string(), "Linus Torvalds".to_string()),
            ]),
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
            "href=\"conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=eyes&amp;add=true\" title=\"Grace Hopper: 👀\""
        ));
        assert!(html.contains(
            "conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=eyes&amp;add=true"
        ));
        let reaction_chip = html.find("<a class=\"reaction is-active\"").unwrap();
        assert!(html.contains("title=\"Ada Lovelace, Grace Hopper, Linus Torvalds: 👍\""));
        assert!(html.contains("aria-label=\"Ada Lovelace, Grace Hopper, Linus Torvalds: 👍\""));
        assert!(html.contains("conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=thumbsup&amp;add=false"));
        let thread_chip = html.find("<a class=\"reaction thread-reaction\"").unwrap();
        assert!(reaction_chip < thread_chip);
        assert!(html.contains("conduit://save?channel=C123&amp;ts=1710000000.000100&amp;add=false"));
        assert!(html.contains("conduit://copy-link?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("conduit://copy-message?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("conduit://forward?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("data-open-emoji-picker"));
        assert_eq!(html.matches("id=\"emoji-picker\"").count(), 1);
        assert!(html.contains("aria-labelledby=\"emoji-picker-title\""));
        assert!(html.contains("id=\"emoji-search\""));
        assert!(html.contains("split(/\\s+/).filter(Boolean)"));
        assert!(html.contains("function choiceMatchScore(choice, terms)"));
        assert!(html.contains("70 * mean + 30 * Math.min(...termScores)"));
        assert!(html.contains("data-original-index="));
        assert!(html.contains("grid.appendChild(choice)"));
        assert!(html.contains("role=\"tablist\""));
        assert!(html.contains("class=\"emoji-grid\""));
        assert!(html.contains("role=\"menu\""));
        assert!(html.contains("if (menu) menu.open = false"));
        let quick_actions = &html[html.find("<nav class=\"quick-actions\"").unwrap()..];
        let quick_actions = &quick_actions[..quick_actions.find("</nav>").unwrap()];
        let recent = quick_actions.find("name=heart").unwrap();
        let picker = quick_actions.find("data-open-emoji-picker").unwrap();
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
    fn reaction_chip_toggle_preserves_thread_context() {
        let mut reply = message("reply");
        reply.thread_ts = Some("1710000000.000000".to_string());
        reply.reactions = Some(vec![SlackReaction {
            name: Some("party_parrot".to_string()),
            count: Some(1),
            users: Some(vec!["U456".to_string()]),
        }]);
        let context = MessageHtmlContext {
            current_user_id: Some("U999".to_string()),
            user_names: HashMap::from([("U456".to_string(), "Grace Hopper".to_string())]),
            custom_emojis: HashMap::from([(
                "party_parrot".to_string(),
                "https://example.com/party-parrot.gif".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[reply], &context);

        assert!(html.contains(
            "href=\"conduit://reaction?channel=C123&amp;ts=1710000000.000100&amp;name=party_parrot&amp;add=true&amp;thread_ts=1710000000.000000\""
        ));
        assert!(html.contains("title=\"Grace Hopper: :party_parrot:\""));
    }

    #[test]
    fn emoji_picker_cancellation_is_shared_and_restores_focus() {
        let html = conversation_document(
            "C123",
            &[message("Pick a reaction")],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains("function cancelPicker(event)"));
        assert!(html.contains("event.preventDefault();"));
        assert!(html.contains("event.stopPropagation();"));
        assert!(html.contains("picker.close(\"cancel\")"));
        assert!(html.contains(
            "picker.querySelector(\".picker-close\").addEventListener(\"click\", cancelPicker)"
        ));
        assert!(html.contains("picker.addEventListener(\"cancel\", cancelPicker)"));
        assert!(html.contains(
            "if (!picker.open || (event.key !== \"Escape\" && event.key !== \"Esc\")) return"
        ));
        assert!(html.contains("cancelPicker(event);"));
        assert!(html.contains("if (event.target !== picker) return"));
        assert!(html.contains("if (!inside) cancelPicker(event)"));
        assert!(html.contains("if (opener) opener.focus()"));
        assert!(html.contains("Close emoji picker"));
        assert!(!html.contains("reaction-picker"));
    }

    #[test]
    fn emoji_picker_exposes_shared_accessibility_and_keyboard_navigation() {
        let html = conversation_document(
            "C123",
            &[message("Pick a reaction")],
            &MessageHtmlContext::default(),
        );

        assert!(html.contains("role=\"combobox\""));
        assert!(html.contains("aria-controls=\"emoji-grid\""));
        assert!(html.contains("role=\"gridcell\""));
        assert!(html.contains("aria-selected=\"false\""));
        assert!(html.contains("function moveSelection(offset)"));
        assert!(html.contains("event.key === \"ArrowUp\" || event.key === \"ArrowDown\""));
        assert!(html.contains("moveSelection(event.key === \"ArrowUp\" ? -1 : 1)"));
        assert!(html.contains("event.key === \"Enter\" && selectedChoice"));
        assert!(html.contains("activateChoice(selectedChoice)"));
        assert!(html.contains("search.setAttribute(\"aria-activedescendant\", selectedChoice.id)"));
        assert!(html.contains(".emoji-choice[aria-selected=\"true\"]"));
    }

    #[test]
    fn unread_conversation_advances_to_the_newest_visible_message() {
        let context = MessageHtmlContext {
            read_marker_url: Some(mark_read_action_url("C123", "1710000000.000100")),
            first_unread_ts: Some("1710000000.000100".into()),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("unread")], &context);

        assert!(html.contains("class=\"unread-separator\""));
        assert!(html.contains("new IntersectionObserver"));
        assert!(html.contains("target.searchParams.set(\"ts\", newest)"));
        assert!(html.contains("if (separator && next) next.before(separator)"));
    }

    #[test]
    fn thread_read_marker_and_scroll_state_are_scoped_to_the_thread() {
        let context = MessageHtmlContext {
            thread_ts: Some("1710000000.000100".into()),
            read_marker_url: Some(mark_thread_read_action_url(
                "C123",
                "1710000000.000100",
                "1710000001.000200",
            )),
            timeline_scroll: TimelineScrollBehavior::StickToBottom,
            ..Default::default()
        };

        let html = conversation_document("C123", &[message("reply")], &context);

        assert!(html.contains("id=\"timeline-read-sentinel\""));
        assert!(html.contains("thread_ts=1710000000.000100"));
        assert!(html.contains("conduit:timeline-at-bottom:thread:C123:1710000000.000100"));
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
    fn unsupported_attachment_downloads_private_url_through_internal_action() {
        let mut message = message("document");
        message.files = Some(vec![SlackFile {
            title: Some("Quarterly report".to_string()),
            name: Some("quarterly-report.pdf".to_string()),
            mimetype: Some("application/pdf".to_string()),
            permalink: Some("https://workspace.slack.com/files/U1/F1".to_string()),
            url_private: Some("https://files.slack.com/files-pri/F1/report.pdf".to_string()),
            url_private_download: Some(
                "https://files.slack.com/files-pri/F1/download/report.pdf".to_string(),
            ),
            ..Default::default()
        }]);

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("conduit://attachment?"));
        assert!(html.contains(
            "url=https%3A%2F%2Ffiles.slack.com%2Ffiles-pri%2FF1%2Fdownload%2Freport.pdf"
        ));
        assert!(html.contains("name=quarterly-report.pdf"));
        assert!(html.contains("Attachment: Quarterly report"));
        assert!(!html.contains("workspace.slack.com/files/U1/F1"));
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
    fn renders_video_attachment_thumbnail_linked_to_internal_viewer() {
        let preview = "https://files.slack.com/video-preview.png";
        let original = "https://files.slack.com/video.mp4";
        let mut message = message("video");
        message.files = Some(vec![SlackFile {
            title: Some("Demo clip".to_string()),
            mimetype: Some("video/mp4".to_string()),
            url_private_download: Some(original.to_string()),
            url_static_preview: Some(preview.to_string()),
            ..Default::default()
        }]);
        let context = MessageHtmlContext {
            image_assets: HashMap::from([(
                preview.to_string(),
                "data:image/png;base64,video-poster".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("class=\"video-attachment\""));
        assert!(html.contains("src=\"data:image/png;base64,video-poster\""));
        assert!(html.contains("alt=\"Video preview: Demo clip\""));
        assert!(html.contains("aria-label=\"Play video: Demo clip\""));
        assert!(html.contains("kind=video"));
        assert!(html.contains("url=https%3A%2F%2Ffiles.slack.com%2Fvideo.mp4"));
        assert!(html.contains("class=\"video-play\" aria-hidden=\"true\""));
    }

    #[test]
    fn renders_video_preview_placeholder_while_thumbnail_loads() {
        let mut message = message("video");
        message.files = Some(vec![SlackFile {
            title: Some("Demo clip".to_string()),
            mimetype: Some("video/mp4".to_string()),
            url_private: Some("https://files.slack.com/video.mp4".to_string()),
            thumb_360: Some("https://files.slack.com/video-preview.png".to_string()),
            ..Default::default()
        }]);

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("class=\"video-attachment\""));
        assert!(html.contains("Loading video preview"));
    }

    #[test]
    fn video_without_static_thumbnail_uses_bounded_video_preview_asset() {
        let mut message = message("video");
        message.files = Some(vec![SlackFile {
            title: Some("Demo clip".to_string()),
            mimetype: Some("video/mp4".to_string()),
            url_private: Some("https://files.slack.com/video.mp4".to_string()),
            ..Default::default()
        }]);

        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("class=\"video-attachment\""));
        assert!(html.contains("Loading video preview"));
        assert!(html.contains("kind=video"));
    }

    #[test]
    fn renders_loaded_motion_video_preview() {
        let preview = "https://files.slack.com/video-preview.mp4";
        let mut message = message("video");
        message.files = Some(vec![SlackFile {
            title: Some("Demo clip".to_string()),
            mimetype: Some("video/mp4".to_string()),
            url_private_download: Some("https://files.slack.com/original.mp4".to_string()),
            thumb_video: Some(preview.to_string()),
            ..Default::default()
        }]);
        let context = MessageHtmlContext {
            image_assets: HashMap::from([(
                preview.to_string(),
                "data:video/mp4;base64,motion-preview".to_string(),
            )]),
            ..Default::default()
        };

        let html = conversation_document("C123", &[message], &context);

        assert!(html.contains("<video preload=\"metadata\" muted playsinline"));
        assert!(html.contains("src=\"data:video/mp4;base64,motion-preview\""));
        assert!(html.contains("aria-label=\"Video preview: Demo clip\""));
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
            thread_ts: None,
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
    fn unreads_document_links_thread_activity_to_the_thread() {
        let html = unreads_document(&[ActivityItem {
            channel_id: "C123".to_string(),
            thread_ts: Some("1710000000.000100".to_string()),
            title: "#general: Deployment".to_string(),
            kind: ActivityKind::Thread,
            unread: true,
            unread_count: 2,
        }]);

        assert!(html.contains("conduit://thread?channel=C123&amp;ts=1710000000.000100"));
        assert!(html.contains("Thread"));
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
    fn search_results_resolve_dm_group_dm_and_author_display_names() {
        let results = vec![
            SearchMatch {
                channel: Some(crate::models::SlackSearchChannel {
                    id: Some("D123".to_string()),
                    name: Some("directmessage".to_string()),
                }),
                user: Some("U_AUTHOR".to_string()),
                username: Some("legacy-name".to_string()),
                text: Some("Direct result".to_string()),
                ..Default::default()
            },
            SearchMatch {
                channel: Some(crate::models::SlackSearchChannel {
                    id: Some("G123".to_string()),
                    name: Some("mpdm-ada-grace-1".to_string()),
                }),
                text: Some("Group result".to_string()),
                ..Default::default()
            },
        ];
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U_AUTHOR".to_string(), "Linus Torvalds".to_string())]),
            conversation_titles: HashMap::from([
                ("D123".to_string(), "Ada Lovelace".to_string()),
                ("G123".to_string(), "Ada Lovelace, Grace Hopper".to_string()),
            ]),
            ..Default::default()
        };

        let html = search_results_document(&results, &context);

        assert!(html.contains("Ada Lovelace"));
        assert!(html.contains("Ada Lovelace, Grace Hopper"));
        assert!(html.contains("Linus Torvalds"));
        assert!(!html.contains("#directmessage"));
        assert!(!html.contains("#mpdm-ada-grace-1"));
        assert!(!html.contains("legacy-name"));
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
        assert!(!html.contains("target.focus"));
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
        assert_eq!(search.matches("<ol class=\"message-list\">").count(), 1);
        assert!(!search.contains("<section"));
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

    #[test]
    fn conversation_document_installs_incremental_runtime_and_stable_regions() {
        let html =
            conversation_document("C123", &[message("hello")], &MessageHtmlContext::default());

        assert!(html.contains("window.conduitApplyTimelinePatch"));
        assert!(html.contains("preserveViewportAnchorDuringResize"));
        assert!(html.contains("new ResizeObserver"));
        assert!(html.contains("const offset = currentTop - viewportAnchorTop"));
        assert!(html.contains("if (Math.abs(offset) <= 0.5) return"));
        assert!(html.contains("root.scrollTop += offset"));
        assert!(html.contains("event.target.closest(\"[data-message-ts]\")"));
        assert!(html.contains("data-author-user-id=\"U123\""));
        for region in ["body", "attachments", "responses"] {
            assert!(html.contains(&format!("data-message-region=\"{region}\"")));
        }
        assert!(html.contains("withPreservedScroll"));
        assert!(html.contains("stableAnchor.getBoundingClientRect().top - anchorTop"));
        assert!(html.contains("viewportPinnedToBottom"));
        assert!(html.contains("bottomThreshold = 96"));
        assert!(html.contains("mode = \"preserve\""));
        assert!(html.contains("window.addEventListener(\"scroll\", rememberViewport"));
        assert!(html.contains("patch.source.startsWith(\"data:video/\")"));
        assert!(html.contains("document.createElement(\"video\")"));
        assert!(html.contains("window.conduitLocalizeTimestamps(template.content)"));
    }

    #[test]
    fn patch_call_serializes_untrusted_values_as_javascript_data() {
        let patch = update_user_patch(
            "U\"123",
            "Ada & </script><script>alert(1)</script>\u{2028}",
            Some(&SlackUserStatus {
                text: "A&B".into(),
                emoji: ":wave:".into(),
                ..Default::default()
            }),
            &HashMap::new(),
        );
        let script = timeline_dom_patch_call(&patch);

        assert!(script
            .starts_with("window.conduitApplyTimelinePatch ? window.conduitApplyTimelinePatch({"));
        assert!(script.contains("U\\\"123"));
        assert!(script.contains("\\u003c/script\\u003e"));
        assert!(script.contains("\\u0026"));
        assert!(script.contains("\\u2028"));
        assert!(!script.contains("</script>"));
    }

    #[test]
    fn message_patch_helpers_render_escaped_standalone_and_region_html() {
        let mut context = MessageHtmlContext::default();
        context
            .user_names
            .insert("U123".into(), "Ada <Admin>".into());
        let message = message("Hello <everyone>");

        let inserted =
            insert_message_patch("C123", &message, &context, TimelineInsertPosition::Append);
        let TimelineDomPatch::InsertMessage { position, html } = inserted else {
            panic!("expected insert patch");
        };
        assert_eq!(position, TimelineInsertPosition::Append);
        assert!(html.starts_with("<li class=\"message-list-item\"><article"));
        assert!(html.contains("Ada &lt;Admin&gt;"));
        assert!(html.contains("Hello &lt;everyone&gt;"));
        assert!(html.contains("<time class=\"metadata\""));

        let reactions =
            message_region_patch("C123", &message, &context, TimelineMessageRegion::Responses);
        assert_eq!(
            reactions,
            TimelineDomPatch::ReplaceRegion {
                message_ts: message.ts.clone(),
                region: TimelineMessageRegion::Responses,
                html: String::new(),
            }
        );
    }

    #[test]
    fn image_placeholders_have_stable_escaped_patch_metadata() {
        let file = SlackFile {
            name: Some("preview.png".into()),
            mimetype: Some("image/png".into()),
            url_private: Some("https://files.slack.com/image?<unsafe>".into()),
            ..Default::default()
        };
        let mut message = message("image");
        message.files = Some(vec![file]);
        let html = conversation_document("C123", &[message], &MessageHtmlContext::default());

        assert!(html.contains("data-image-key=\"https://files.slack.com/image?&lt;unsafe&gt;\""));
        assert!(html.contains("data-image-alt="));
        assert!(html.contains("data-image-unavailable="));
    }

    #[test]
    fn author_menu_and_profile_page_expose_person_actions_and_details() {
        let context = MessageHtmlContext {
            user_names: HashMap::from([("U123".into(), "Ada".into())]),
            ..Default::default()
        };
        let html = conversation_document("C123", &[message("hello")], &context);
        assert!(html.contains("conduit://user-message?user=U123"));
        assert!(html.contains("conduit://user-profile?user=U123"));
        assert!(!html.contains("document.addEventListener(\"contextmenu\""));

        let profile = SlackUser {
            id: Some("U123".into()),
            real_name: Some("Ada Lovelace".into()),
            tz_label: Some("Europe/Amsterdam".into()),
            profile: Some(crate::models::SlackUserProfile {
                display_name: Some("Ada :wave:".into()),
                title: Some("Engineer :rocket:".into()),
                email: Some("ada@example.test".into()),
                about: Some("Builds useful things :coffee:".into()),
                fields: HashMap::from([(
                    "X123".into(),
                    crate::models::SlackProfileField {
                        label: Some("Office".into()),
                        value: Some("Amsterdam".into()),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let context = MessageHtmlContext {
            custom_emojis: HashMap::from([(
                "working".into(),
                "https://emoji.slack-edge.com/T123/working.png".into(),
            )]),
            ..Default::default()
        };
        let mut profile = profile;
        profile.profile.as_mut().unwrap().status_emoji = Some(":working:".into());
        profile.profile.as_mut().unwrap().status_text = Some("Focused :coffee:".into());
        let profile_html = user_profile_document(&profile, &context);
        assert!(profile_html.contains("Ada Lovelace"));
        assert!(profile_html.contains("Engineer"));
        assert!(profile_html.contains("ada@example.test"));
        assert!(profile_html.contains("Builds useful things"));
        assert!(profile_html.contains("Europe/Amsterdam"));
        assert!(profile_html.contains("Office"));
        assert!(profile_html.contains("Amsterdam"));
        assert!(profile_html.contains("title=\":working:\""));
        assert!(profile_html.contains("https://emoji.slack-edge.com/T123/working.png"));
        assert!(profile_html.contains("title=\":coffee:\""));
        assert!(profile_html.contains("title=\":wave:\""));
        assert!(profile_html.contains("title=\":rocket:\""));
        assert!(!profile_html.contains(":working: Focused :coffee:"));
    }
}
