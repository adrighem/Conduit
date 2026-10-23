use std::path::PathBuf;

use adw::prelude::*;
use gtk::{glib, pango, Box, Button, Label, Orientation, TextView, Widget};
use sourceview5::prelude::*;

use crate::message_markup::{emoji_only_names, MessageRenderContext, INLINE_EMOJI_SIZE};
use crate::timeline::actions::{ActionHandler, TimelineAction};
use crate::timeline::cache::resolve_cached_asset_path;
use crate::timeline::reactions::{new_chip_wrap_box, resolve_reaction_emoji_widget};
use crate::timeline::row::{load_animated_or_static_picture, register_timeline_css};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCustomEmoji {
    pub name: String,
    pub url: String,
}

pub fn extract_custom_emojis_and_prepare_markup(pango: &str) -> (String, Vec<InlineCustomEmoji>) {
    let mut result = String::with_capacity(pango.len());
    let mut emojis = Vec::new();
    let mut rest = pango;

    while let Some(start) = rest.find("<conduit-custom-emoji ") {
        result.push_str(&rest[..start]);
        let tag_rest = &rest[start..];
        if let Some(end) = tag_rest.find("/>") {
            let tag_content = &tag_rest[..end + 2];
            let name = extract_attr_value(tag_content, "name").unwrap_or_default();
            let url = extract_attr_value(tag_content, "url").unwrap_or_default();
            emojis.push(InlineCustomEmoji { name, url });
            result.push('\u{FFFC}');
            rest = &tag_rest[end + 2..];
        } else {
            result.push_str(&rest[..start + 22]);
            rest = &rest[start + 22..];
        }
    }
    result.push_str(rest);
    (result, emojis)
}

fn unescape_xml_attribute(val: &str) -> String {
    val.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(unescape_xml_attribute(&tag[start..end]))
}

fn create_inline_emoji_widget(emoji: &InlineCustomEmoji, context: &MessageRenderContext) -> Widget {
    let local_path = crate::emoji_picker_window::resolve_custom_emoji_cached_file(&emoji.url)
        .or_else(|| resolve_cached_asset_path(&emoji.url, context));

    if let Some(path) = local_path {
        let widget = load_animated_or_static_picture(
            &path,
            INLINE_EMOJI_SIZE,
            INLINE_EMOJI_SIZE,
            gtk::ContentFit::Contain,
        );
        widget.set_valign(gtk::Align::Center);
        widget.set_tooltip_text(Some(&format!(":{}:", emoji.name)));
        widget
    } else {
        let bin = adw::Bin::new();
        bin.set_valign(gtk::Align::Center);
        bin.set_tooltip_text(Some(&format!(":{}:", emoji.name)));
        let label = Label::new(Some(&format!(":{}:", emoji.name)));
        label.set_selectable(false);
        bin.set_child(Some(&label));

        let bin_weak = bin.downgrade();
        let emoji_name = emoji.name.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        crate::emoji_picker_window::request_custom_emoji_file(&emoji.url, move |path| {
            let _ = tx.send(path);
        });
        glib::spawn_future_local(async move {
            let Some(path) = rx.recv().await else {
                return;
            };
            if let Some(bin) = bin_weak.upgrade() {
                let pic = load_animated_or_static_picture(
                    &path,
                    INLINE_EMOJI_SIZE,
                    INLINE_EMOJI_SIZE,
                    gtk::ContentFit::Contain,
                );
                pic.set_valign(gtk::Align::Center);
                pic.set_tooltip_text(Some(&format!(":{}:", emoji_name)));
                bin.set_child(Some(&pic));
            }
        });

        bin.upcast::<Widget>()
    }
}

/// A link extracted from Pango markup, as a char range of the visible text.
#[derive(Debug, PartialEq)]
pub struct MarkupLink {
    pub start: i32,
    pub end: i32,
    pub href: String,
}

/// Removes `<a href>` tags, which are only valid in GtkLabel markup, and returns the
/// links as char ranges into the text that results from parsing the stripped markup.
pub fn strip_anchor_tags(markup: &str) -> (String, Vec<MarkupLink>) {
    let mut out = String::with_capacity(markup.len());
    let mut links = Vec::new();
    let mut offset = 0i32;
    let mut open: Option<(i32, String)> = None;
    let mut rest = markup;

    while let Some(pos) = rest.find(['<', '&']) {
        let (text, tail) = rest.split_at(pos);
        out.push_str(text);
        offset += text.chars().count() as i32;
        if tail.starts_with('&') {
            let len = tail.find(';').map_or(1, |i| i + 1);
            out.push_str(&tail[..len]);
            offset += 1;
            rest = &tail[len..];
            continue;
        }
        let Some(close) = tail.find('>') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let tag = &tail[..=close];
        if tag.starts_with("<a ") || tag == "<a>" {
            let href = extract_attr_value(tag, "href").unwrap_or_default();
            open = Some((offset, href));
        } else if tag == "</a>" {
            if let Some((start, href)) = open.take() {
                if !href.is_empty() && offset > start {
                    links.push(MarkupLink {
                        start,
                        end: offset,
                        href,
                    });
                }
            }
        } else {
            out.push_str(tag);
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    (out, links)
}

/// The action for a clicked non-user link: user group members or open URL.
fn link_action(href: &str) -> TimelineAction {
    match href.strip_prefix(crate::message_markup::USER_GROUP_URL_PREFIX) {
        Some(group_id) => TimelineAction::ShowUserGroup(group_id.to_string()),
        None => TimelineAction::OpenLink(href.to_string()),
    }
}

pub(crate) fn attach_label_links(
    label: &Label,
    context: &MessageRenderContext,
    on_action: Option<&ActionHandler>,
) {
    let context = context.clone();
    let on_action = on_action.cloned();
    label.connect_activate_link(move |label, uri| {
        if let Some(user_id) = uri.strip_prefix("conduit-user://") {
            if let Some(on_action) = on_action.as_ref() {
                crate::author_menu::show_user_mention_popover(
                    label,
                    user_id,
                    &context,
                    on_action.clone(),
                    None,
                );
            }
            glib::Propagation::Stop
        } else if let Some(on_action) = on_action.as_ref() {
            on_action(link_action(uri));
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Underlines link ranges in a TextView buffer and opens them on click.
fn attach_text_view_links(
    view: &TextView,
    links: Vec<MarkupLink>,
    context: &MessageRenderContext,
    on_action: Option<&ActionHandler>,
) {
    if links.is_empty() {
        return;
    }
    let buffer = view.buffer();
    let mut hrefs = Vec::with_capacity(links.len());
    for (index, link) in links.into_iter().enumerate() {
        let name = format!("conduit-link-{index}");
        let tag = gtk::TextTag::builder()
            .name(name.as_str())
            .underline(pango::Underline::Single)
            .build();
        buffer.tag_table().add(&tag);
        buffer.apply_tag(
            &tag,
            &buffer.iter_at_offset(link.start),
            &buffer.iter_at_offset(link.end),
        );
        hrefs.push((name, link.href));
    }
    let click = gtk::GestureClick::new();
    let context = context.clone();
    let on_action = on_action.cloned();
    // Resolve view from gesture: capturing view form strong cycle and leak view.
    click.connect_released(move |gesture, _, x, y| {
        let Some(click_view) = gesture.widget().and_downcast::<TextView>() else {
            return;
        };
        let (bx, by) =
            click_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let Some(iter) = click_view.iter_at_location(bx, by) else {
            return;
        };
        for tag in iter.tags() {
            let Some(tag_name) = tag.name() else { continue };
            if let Some((_, href)) = hrefs.iter().find(|(n, _)| n.as_str() == tag_name.as_str()) {
                if let Some(user_id) = href.strip_prefix("conduit-user://") {
                    if let Some(on_action) = on_action.as_ref() {
                        let rect = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
                        crate::author_menu::show_user_mention_popover(
                            &click_view,
                            user_id,
                            &context,
                            on_action.clone(),
                            Some(rect),
                        );
                    }
                } else if let Some(on_action) = on_action.as_ref() {
                    on_action(link_action(href));
                } else {
                    let _ = gtk::gio::AppInfo::launch_default_for_uri(
                        href,
                        None::<&gtk::gio::AppLaunchContext>,
                    );
                }
                break;
            }
        }
    });
    view.add_controller(click);
}

pub(crate) fn create_message_text_widget(
    pango: &str,
    context: &MessageRenderContext,
    on_action: Option<&ActionHandler>,
) -> Widget {
    if !pango.contains("<conduit-custom-emoji ") {
        let label = Label::new(None);
        label.set_wrap(true);
        label.set_wrap_mode(pango::WrapMode::WordChar);
        label.set_selectable(true);
        label.set_focus_on_click(false);
        label.set_xalign(0.0);
        // libadwaita's document font (family, size, line height) for reading content.
        label.add_css_class("document");
        label.set_markup(pango);
        attach_label_links(&label, context, on_action);
        return label.upcast::<Widget>();
    }

    let (markup_clean, emojis) = extract_custom_emojis_and_prepare_markup(pango);
    let (markup_clean, links) = strip_anchor_tags(&markup_clean);
    let view = TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.add_css_class("timeline-text-view");
    view.add_css_class("document");
    register_timeline_css();

    let buffer = view.buffer();
    let mut iter = buffer.start_iter();
    buffer.insert_markup(&mut iter, &markup_clean);
    attach_text_view_links(&view, links, context, on_action);

    let mut search_iter = buffer.start_iter();
    for emoji in &emojis {
        if let Some((start_match, end_match)) =
            search_iter.forward_search("\u{FFFC}", gtk::TextSearchFlags::empty(), None)
        {
            let mut del_start = start_match;
            let mut del_end = end_match;
            buffer.delete(&mut del_start, &mut del_end);
            let anchor = buffer.create_child_anchor(&mut del_start);
            let emoji_widget = create_inline_emoji_widget(emoji, context);
            view.add_child_at_anchor(&emoji_widget, &anchor);
            search_iter = del_start;
        } else {
            break;
        }
    }

    view.upcast::<Widget>()
}

#[derive(Debug, PartialEq)]
pub enum TextSegment {
    CodeBlock { lang: Option<String>, code: String },
    Quote(String),
    Normal(String),
}

pub fn parse_text_segments(input: &str) -> Vec<TextSegment> {
    let mut segments = Vec::new();
    let mut remaining = input;

    while !remaining.is_empty() {
        if let Some(fence_start) = remaining.find("```") {
            let before = &remaining[..fence_start];
            if !before.is_empty() {
                parse_quotes_and_normal(before, &mut segments);
            }
            let after_fence = &remaining[fence_start + 3..];
            if let Some(fence_end) = after_fence.find("```") {
                let code_content = &after_fence[..fence_end];
                let (lang, code) = if let Some((first_line, rest)) = code_content.split_once('\n') {
                    let first_line_trimmed = first_line.trim();
                    if !first_line_trimmed.is_empty() && !first_line_trimmed.contains(' ') {
                        (Some(first_line_trimmed.to_string()), rest)
                    } else {
                        (None, code_content)
                    }
                } else {
                    (None, code_content)
                };
                segments.push(TextSegment::CodeBlock {
                    lang,
                    code: code.trim_matches('\n').to_string(),
                });
                remaining = &after_fence[fence_end + 3..];
            } else {
                segments.push(TextSegment::CodeBlock {
                    lang: None,
                    code: after_fence.trim_matches('\n').to_string(),
                });
                break;
            }
        } else {
            parse_quotes_and_normal(remaining, &mut segments);
            break;
        }
    }

    segments
}

fn parse_quotes_and_normal(text: &str, segments: &mut Vec<TextSegment>) {
    if let Some(triple_idx) = text.find(">>>") {
        let before = &text[..triple_idx];
        if !before.is_empty() {
            parse_line_by_line_quotes(before, segments);
        }
        let quote_content = &text[triple_idx + 3..];
        let cleaned_quote = quote_content.trim_start_matches('\n').to_string();
        if !cleaned_quote.is_empty() {
            segments.push(TextSegment::Quote(cleaned_quote));
        }
        return;
    }

    parse_line_by_line_quotes(text, segments);
}

fn parse_line_by_line_quotes(text: &str, segments: &mut Vec<TextSegment>) {
    let mut normal_acc = String::new();
    let mut quote_acc = String::new();

    for line in text.lines() {
        if let Some(stripped) = line.strip_prefix("> ") {
            if !normal_acc.is_empty() {
                segments.push(TextSegment::Normal(normal_acc.clone()));
                normal_acc.clear();
            }
            if !quote_acc.is_empty() {
                quote_acc.push('\n');
            }
            quote_acc.push_str(stripped);
        } else if line == ">" {
            if !normal_acc.is_empty() {
                segments.push(TextSegment::Normal(normal_acc.clone()));
                normal_acc.clear();
            }
            if !quote_acc.is_empty() {
                quote_acc.push('\n');
            }
        } else {
            if !quote_acc.is_empty() {
                segments.push(TextSegment::Quote(quote_acc.clone()));
                quote_acc.clear();
            }
            if !normal_acc.is_empty() {
                normal_acc.push('\n');
            }
            normal_acc.push_str(line);
        }
    }

    if !quote_acc.is_empty() {
        segments.push(TextSegment::Quote(quote_acc));
    }
    if !normal_acc.is_empty() {
        segments.push(TextSegment::Normal(normal_acc));
    }
}

fn resolve_sourceview_language(lang: &str) -> Option<sourceview5::Language> {
    let lower = lang.trim().to_ascii_lowercase();
    let normalized = match lower.as_str() {
        "rs" => "rust",
        "js" => "javascript",
        "ts" => "typescript",
        "py" => "python",
        "sh" | "bash" => "sh",
        "yml" | "yaml" => "yaml",
        "json" => "json",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "md" => "markdown",
        _ => lower.as_str(),
    };
    let mgr = sourceview5::LanguageManager::default();
    mgr.language(normalized).or_else(|| {
        if normalized == "javascript" {
            mgr.language("js")
        } else if normalized == "js" {
            mgr.language("javascript")
        } else {
            None
        }
    })
}

fn resolve_sourceview_style_scheme() -> Option<sourceview5::StyleScheme> {
    let mgr = sourceview5::StyleSchemeManager::default();
    let is_dark = adw::StyleManager::default().is_dark();
    let preferred = if is_dark { "Adwaita-dark" } else { "Adwaita" };
    let fallback = if is_dark { "classic-dark" } else { "classic" };
    mgr.scheme(preferred).or_else(|| mgr.scheme(fallback))
}

pub fn build_code_block_widget(code: &str, lang: Option<&str>) -> Widget {
    register_timeline_css();
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_text(code);

    if let Some(lang_str) = lang {
        if let Some(language) = resolve_sourceview_language(lang_str) {
            buffer.set_language(Some(&language));
        }
    }

    if let Some(scheme) = resolve_sourceview_style_scheme() {
        buffer.set_style_scheme(Some(&scheme));
    }

    let view = sourceview5::View::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    let line_count = code.lines().count();
    view.set_show_line_numbers(line_count > 1);
    view.set_wrap_mode(gtk::WrapMode::WordChar);

    let container = Box::new(Orientation::Vertical, 4);
    container.add_css_class("code-block");
    container.add_css_class("monospace");

    let is_multi_line = line_count > 1;
    if lang.is_some() || is_multi_line {
        let header = Box::new(Orientation::Horizontal, 8);
        header.add_css_class("code-block-header");

        let badge_text = lang.unwrap_or("");
        let badge_label = Label::new(Some(badge_text));
        badge_label.add_css_class("code-block-lang");
        badge_label.add_css_class("dim-label");
        badge_label.set_halign(gtk::Align::Start);
        badge_label.set_hexpand(true);
        header.append(&badge_label);

        let copy_button = Button::from_icon_name("edit-copy-symbolic");
        copy_button.add_css_class("flat");
        copy_button.add_css_class("code-block-copy");
        copy_button.set_tooltip_text(Some("Copy code"));
        copy_button.set_valign(gtk::Align::Center);
        let code_owned = code.to_string();
        copy_button.connect_clicked(move |btn| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&code_owned);
            } else {
                btn.clipboard().set_text(&code_owned);
            }
        });
        header.append(&copy_button);

        container.append(&header);
    }

    container.append(&view);
    container.upcast::<Widget>()
}

/// An emoji-only message: its emoji at reaction size, as Slack does.
fn build_jumbo_emoji_row(names: &[String], context: &MessageRenderContext) -> Widget {
    let catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);
    let row = new_chip_wrap_box();
    row.add_css_class("jumbo-emoji");
    for name in names {
        let widget = resolve_reaction_emoji_widget(name, name, context, &catalog);
        widget.set_tooltip_text(Some(&format!(":{name}:")));
        row.append(&widget);
    }
    row.upcast()
}

pub(crate) fn render_text_content(
    text: &str,
    target_box: &Box,
    context: &MessageRenderContext,
    on_action: Option<&ActionHandler>,
) {
    if let Some(names) = emoji_only_names(text, context) {
        target_box.append(&build_jumbo_emoji_row(&names, context));
        return;
    }
    let segments = parse_text_segments(text);
    for seg in segments {
        match seg {
            TextSegment::CodeBlock { lang, code } => {
                let widget = build_code_block_widget(&code, lang.as_deref());
                target_box.append(&widget);
            }
            TextSegment::Quote(quote_text) => {
                register_timeline_css();
                let quote_box = Box::new(Orientation::Vertical, 0);
                quote_box.add_css_class("blockquote");
                let pango = crate::message_markup::mrkdwn_to_pango(&quote_text, context);
                let text_widget =
                    create_message_text_widget(&format!("<i>{}</i>", pango), context, on_action);
                quote_box.append(&text_widget);
                target_box.append(&quote_box);
            }
            TextSegment::Normal(normal_text) => {
                if !normal_text.trim().is_empty() {
                    let pango = crate::message_markup::mrkdwn_to_pango(&normal_text, context);
                    let text_widget = create_message_text_widget(&pango, context, on_action);
                    target_box.append(&text_widget);
                }
            }
        }
    }
}
