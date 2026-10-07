//! Native GTK rendering of a [`MessageDocument`], the canonical message
//! content shared with the HTML renderer (`message_html::rich_components`).
//!
//! Cached messages keep only the document (raw blocks and attachments are
//! discarded on save), so this is the single content path for the native
//! timeline; files and reactions are rendered by the caller.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use adw::prelude::*;
use gtk::{glib, pango, Box, Button, CssProvider, Grid, Label, Orientation, Separator, Widget};

use crate::message_html::MessageHtmlContext;
use crate::rich_message::{
    MessageAccessory, MessageAttachment, MessageContextElement, MessageControl, MessageDocument,
    MessageField, MessageImage, MessageLinkedText, MessageNode, MessageQuote, RichInline,
    RichInlineStyle, RichTextNode,
};
use crate::timeline_media::{context_icon, fetchable_url, image_block, MediaSources};
use crate::timeline_message_widget::{
    create_message_text_widget, new_chip_wrap_box, register_timeline_css, render_text_content,
};

const ACCESSORY_IMAGE_SIZE: i32 = 72;
const QUOTE_AVATAR_SIZE: i32 = 20;

thread_local! {
    // Attachment accent colors already registered as CSS classes; GTK has no
    // inline styles, so each distinct normalized color gets one rule.
    static ACCENT_CLASSES: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// True when the document carries the message body itself. Attachments and
/// quote unfurls alone do not: Slack then shows `text` above them, while
/// block-based messages treat `text` as a notification fallback only.
pub(crate) fn document_replaces_text(document: &MessageDocument) -> bool {
    document
        .nodes()
        .iter()
        .any(|node| !matches!(node, MessageNode::Attachment(_) | MessageNode::Quote(_)))
}

pub(crate) struct DocumentRenderer<'a> {
    context: &'a MessageHtmlContext,
    sources: MediaSources<'a>,
    ts: &'a str,
    on_action: Option<&'a crate::timeline_message_widget::ActionHandler>,
    next_media_slot: Cell<usize>,
}

impl<'a> DocumentRenderer<'a> {
    pub(crate) fn new(
        context: &'a MessageHtmlContext,
        sources: MediaSources<'a>,
        ts: &'a str,
        on_action: Option<&'a crate::timeline_message_widget::ActionHandler>,
    ) -> Self {
        Self {
            context,
            sources,
            ts,
            on_action,
            next_media_slot: Cell::new(0),
        }
    }

    pub(crate) fn render(&self, nodes: &[MessageNode], target: &Box) {
        for node in nodes {
            self.render_node(node, target);
        }
    }

    fn render_node(&self, node: &MessageNode, target: &Box) {
        match node {
            MessageNode::Text(text) => render_text_content(text, target, self.context),
            MessageNode::Header(text) => target.append(&markup_label(&format!(
                "<span size=\"larger\" weight=\"bold\">{}</span>",
                glib::markup_escape_text(text)
            ))),
            MessageNode::Section {
                text,
                fields,
                accessory,
            } => self.render_section(text.as_deref(), fields, accessory.as_ref(), target),
            MessageNode::Context(elements) => target.append(&self.context_row(elements)),
            MessageNode::Divider => target.append(&Separator::new(Orientation::Horizontal)),
            MessageNode::Image(image) => target.append(&self.image(image)),
            MessageNode::Control(control) => {
                target.append(&self.controls_row(std::slice::from_ref(control)))
            }
            MessageNode::Actions(controls) => target.append(&self.controls_row(controls)),
            MessageNode::RichText(nodes) => {
                for node in nodes {
                    self.render_rich_text(node, target);
                }
            }
            MessageNode::Attachment(attachment) => self.render_attachment(attachment, target),
            MessageNode::Quote(quote) => target.append(&self.quote(quote)),
            MessageNode::Unsupported { fallback, .. } => {
                if let Some(fallback) = fallback.as_deref().filter(|text| !text.trim().is_empty()) {
                    let label = Label::new(Some(fallback));
                    label.add_css_class("dim-label");
                    label.set_wrap(true);
                    label.set_xalign(0.0);
                    target.append(&label);
                }
            }
        }
    }

    fn image(&self, image: &MessageImage) -> Widget {
        let slot = self.next_media_slot.get();
        self.next_media_slot.set(slot + 1);
        image_block(image, &self.sources, self.ts, &format!("document:{slot}"))
    }

    fn mrkdwn_widget(&self, text: &str) -> Widget {
        create_message_text_widget(
            &crate::message_html::mrkdwn_to_pango(text, self.context),
            self.context,
        )
    }

    fn render_section(
        &self,
        text: Option<&str>,
        fields: &[String],
        accessory: Option<&MessageAccessory>,
        target: &Box,
    ) {
        let row = Box::new(Orientation::Horizontal, 12);
        let body = Box::new(Orientation::Vertical, 4);
        body.set_hexpand(true);
        if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
            render_text_content(text, &body, self.context);
        }
        if !fields.is_empty() {
            let widgets = fields
                .iter()
                .map(|field| self.mrkdwn_widget(field))
                .collect();
            body.append(&two_column_grid(widgets));
        }
        row.append(&body);
        match accessory {
            Some(MessageAccessory::Image(image)) => {
                let thumb = context_sized_image(image, &self.sources, ACCESSORY_IMAGE_SIZE);
                thumb.set_valign(gtk::Align::Start);
                row.append(&thumb);
            }
            Some(MessageAccessory::Control(control)) => {
                let button = self.control_button(control);
                button.set_valign(gtk::Align::Start);
                row.append(&button);
            }
            None => {}
        }
        target.append(&row);
    }

    fn context_row(&self, elements: &[MessageContextElement]) -> adw::WrapBox {
        let row = new_chip_wrap_box();
        for element in elements {
            match element {
                MessageContextElement::Image(image) => {
                    row.append(&context_icon(image, &self.sources))
                }
                MessageContextElement::Text(text) => {
                    let pango = crate::message_html::mrkdwn_to_pango(text, self.context);
                    let widget = create_message_text_widget(
                        &format!("<span size=\"small\">{pango}</span>"),
                        self.context,
                    );
                    widget.add_css_class("dim-label");
                    widget.set_valign(gtk::Align::Center);
                    row.append(&widget);
                }
            }
        }
        row
    }

    fn render_rich_text(&self, node: &RichTextNode, target: &Box) {
        match node {
            RichTextNode::Paragraph(inlines) => {
                render_text_content(&inlines_to_mrkdwn(inlines), target, self.context);
            }
            RichTextNode::Preformatted(inlines) => {
                let frame = Box::new(Orientation::Vertical, 0);
                frame.add_css_class("code-block");
                frame.add_css_class("monospace");
                frame.append(&markup_label(&glib::markup_escape_text(
                    &inlines_plain_text(inlines),
                )));
                target.append(&frame);
            }
            RichTextNode::Quote(inlines) => {
                register_timeline_css();
                let quote = Box::new(Orientation::Vertical, 0);
                quote.add_css_class("blockquote");
                quote.append(&self.mrkdwn_widget(&inlines_to_mrkdwn(inlines)));
                target.append(&quote);
            }
            RichTextNode::List { ordered, items } => {
                let list = Box::new(Orientation::Vertical, 2);
                for (index, item) in items.iter().enumerate() {
                    let prefix = if *ordered {
                        format!("{}. ", index + 1)
                    } else {
                        "\u{2022} ".to_string()
                    };
                    list.append(
                        &self.mrkdwn_widget(&format!("{prefix}{}", inlines_to_mrkdwn(item))),
                    );
                }
                target.append(&list);
            }
        }
    }

    fn render_attachment(&self, attachment: &MessageAttachment, target: &Box) {
        register_timeline_css();
        if let Some(pretext) = attachment.pretext.as_deref() {
            render_text_content(pretext, target, self.context);
        }
        let card = Box::new(Orientation::Vertical, 4);
        card.add_css_class("timeline-attachment");
        if let Some(class) = attachment.color.as_deref().and_then(accent_class) {
            card.add_css_class(&class);
        }
        if let Some(author) = attachment.author.as_ref() {
            card.append(&linked_label(author, "small", false));
        }
        if let Some(title) = attachment.title.as_ref() {
            card.append(&linked_label(title, "medium", true));
        }
        if let Some(text) = attachment.text.as_deref() {
            render_text_content(text, &card, self.context);
        }
        if !attachment.fields.is_empty() {
            let widgets = attachment
                .fields
                .iter()
                .map(|field| self.field(field))
                .collect();
            card.append(&two_column_grid(widgets));
        }
        let has_text = attachment.author.is_some()
            || attachment.title.is_some()
            || attachment.text.is_some()
            || !attachment.fields.is_empty();
        if !has_text {
            if let Some(fallback) = attachment.fallback.as_deref() {
                render_text_content(fallback, &card, self.context);
            }
        }
        // Unfurl previews on the linked site's own host are never fetched
        // (privacy allowlist); the card's title and text already describe
        // the link, so omit the image instead of an "Image: ..." line.
        if let Some(image) = attachment
            .image
            .as_ref()
            .filter(|image| fetchable_url(image).is_some())
        {
            card.append(&self.image(image));
        }
        if !attachment.actions.is_empty() {
            card.append(&self.controls_row(&attachment.actions));
        }
        if attachment.footer.is_some() || attachment.footer_icon.is_some() {
            card.append(&self.attachment_footer(attachment));
        }
        target.append(&card);
    }

    fn attachment_footer(&self, attachment: &MessageAttachment) -> Box {
        let row = Box::new(Orientation::Horizontal, 4);
        if let Some(icon) = attachment.footer_icon.as_deref() {
            let image = MessageImage::new(Some(icon.to_string()), "", None);
            row.append(&context_icon(&image, &self.sources));
        }
        if let Some(footer) = attachment.footer.as_deref() {
            let pango = crate::message_html::mrkdwn_to_pango(footer, self.context);
            let label = markup_label(&format!("<span size=\"small\">{pango}</span>"));
            label.add_css_class("dim-label");
            row.append(&label);
        }
        row
    }

    fn field(&self, field: &MessageField) -> Widget {
        let value = field
            .value
            .as_deref()
            .map(|value| crate::message_html::mrkdwn_to_pango(value, self.context));
        let markup = match (field.title.as_deref(), value) {
            (Some(title), Some(value)) => {
                format!("<b>{}</b>\n{value}", glib::markup_escape_text(title))
            }
            (Some(title), None) => format!("<b>{}</b>", glib::markup_escape_text(title)),
            (None, Some(value)) => value,
            (None, None) => String::new(),
        };
        create_message_text_widget(&markup, self.context)
    }

    fn quote(&self, quote: &MessageQuote) -> Box {
        register_timeline_css();
        let card = Box::new(Orientation::Vertical, 4);
        card.add_css_class("timeline-attachment");

        let header = Box::new(Orientation::Horizontal, 6);
        let avatar_url = quote
            .author_id
            .as_deref()
            .and_then(|user_id| self.context.user_avatar_urls.get(user_id))
            .map(String::as_str)
            .or(quote.author_icon.as_deref());
        if let Some(url) = avatar_url {
            let image = MessageImage::new(Some(url.to_string()), "", None);
            header.append(&context_sized_image(
                &image,
                &self.sources,
                QUOTE_AVATAR_SIZE,
            ));
        }
        let author = quote
            .author_id
            .as_deref()
            .and_then(|user_id| self.context.user_names.get(user_id))
            .map(String::as_str)
            .or(quote.author_name.as_deref())
            .unwrap_or("Unknown");
        let mut header_markup = format!("<b>{}</b>", glib::markup_escape_text(author));
        if let Some(channel_id) = quote.channel_id.as_deref() {
            let channel = self
                .context
                .conversation_titles
                .get(channel_id)
                .map(String::as_str)
                .unwrap_or(channel_id);
            header_markup.push_str(&format!(
                " <span size=\"small\">in #{}</span>",
                glib::markup_escape_text(channel.trim_start_matches('#'))
            ));
        }
        header.append(&markup_label(&header_markup));
        card.append(&header);

        self.render(&quote.body, &card);

        if let Some(footer) = quote.footer.as_deref() {
            let markup = match quote
                .permalink_url
                .as_deref()
                .filter(|url| is_http_url(url))
            {
                Some(url) => format!(
                    "<span size=\"small\"><a href=\"{}\">{}</a></span>",
                    glib::markup_escape_text(url),
                    glib::markup_escape_text(footer)
                ),
                None => format!(
                    "<span size=\"small\">{}</span>",
                    glib::markup_escape_text(footer)
                ),
            };
            let label = markup_label(&markup);
            label.add_css_class("dim-label");
            card.append(&label);
        }
        card
    }
}

/// Converts rich-text inlines back to Slack mrkdwn so they share the plain
/// text pipeline (`mrkdwn_to_pango`) with `text`-only messages.
pub(crate) fn inlines_to_mrkdwn(inlines: &[RichInline]) -> String {
    inlines
        .iter()
        .map(|inline| match inline {
            RichInline::Text { text, style } => styled(&escape_mrkdwn(text), *style),
            RichInline::Link { url, label, style } => {
                styled(&format!("<{url}|{}>", escape_mrkdwn(label)), *style)
            }
            RichInline::User(user_id) => format!("<@{user_id}>"),
            RichInline::Channel(channel_id) => format!("<#{channel_id}>"),
            RichInline::Emoji(name) => format!(":{name}:"),
        })
        .collect()
}

fn inlines_plain_text(inlines: &[RichInline]) -> String {
    inlines
        .iter()
        .map(|inline| match inline {
            RichInline::Text { text, .. } => text.clone(),
            RichInline::Link { label, .. } => label.clone(),
            RichInline::User(user_id) => format!("@{user_id}"),
            RichInline::Channel(channel_id) => format!("#{channel_id}"),
            RichInline::Emoji(name) => format!(":{name}:"),
        })
        .collect()
}

/// Slack escapes these three characters in mrkdwn; rich-text inlines carry
/// them raw, so they must be escaped before reuse as mrkdwn.
fn escape_mrkdwn(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Wraps `text` in mrkdwn style markers. Markers must hug non-space
/// characters, so surrounding whitespace stays outside them.
fn styled(text: &str, style: RichInlineStyle) -> String {
    let core = text.trim();
    if core.is_empty() {
        return text.to_string();
    }
    let leading = &text[..text.len() - text.trim_start().len()];
    let trailing = &text[text.trim_end().len()..];
    let mut core = core.to_string();
    for (enabled, marker) in [
        (style.code, "`"),
        (style.bold, "*"),
        (style.italic, "_"),
        (style.strike, "~"),
    ] {
        if enabled {
            core = format!("{marker}{core}{marker}");
        }
    }
    format!("{leading}{core}{trailing}")
}

fn is_http_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

fn markup_label(markup: &str) -> Label {
    let label = Label::new(None);
    label.set_wrap(true);
    label.set_wrap_mode(pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_focus_on_click(false);
    label.set_xalign(0.0);
    label.set_markup(markup);
    label
}

fn linked_label(linked: &MessageLinkedText, size: &str, bold: bool) -> Label {
    let text = glib::markup_escape_text(&linked.text);
    let text = if bold {
        format!("<b>{text}</b>")
    } else {
        text.to_string()
    };
    let text = match linked.url.as_deref().filter(|url| is_http_url(url)) {
        Some(url) => format!("<a href=\"{}\">{text}</a>", glib::markup_escape_text(url)),
        None => text,
    };
    markup_label(&format!("<span size=\"{size}\">{text}</span>"))
}

fn two_column_grid(widgets: Vec<Widget>) -> Grid {
    let grid = Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(4);
    for (index, widget) in widgets.iter().enumerate() {
        grid.attach(widget, (index % 2) as i32, (index / 2) as i32, 1, 1);
    }
    grid
}

impl<'a> DocumentRenderer<'a> {
    fn controls_row(&self, controls: &[MessageControl]) -> adw::WrapBox {
        let row = adw::WrapBox::builder()
            .child_spacing(6)
            .line_spacing(6)
            .build();
        for control in controls {
            row.append(&self.control_button(control));
        }
        row
    }

    fn control_button(&self, control: &MessageControl) -> Button {
        let button = Button::with_label(control.label());
        button.set_focus_on_click(false);
        if let Some(url) = control.url().filter(|url| is_http_url(url)) {
            let url = url.to_string();
            button.set_tooltip_text(Some(&url));
            button.connect_clicked(move |_| {
                if let Err(error) = gtk::gio::AppInfo::launch_default_for_uri(
                    &url,
                    None::<&gtk::gio::AppLaunchContext>,
                ) {
                    crate::debug::log(
                        "timeline",
                        &format!(
                            "ControlLaunchFailed url={} error={error}",
                            crate::debug::url_for_log(&url)
                        ),
                    );
                }
            });
        } else if let Some(key) = control.key() {
            if let Some(on_action) = self.on_action {
                let on_action = on_action.clone();
                let ts = self.ts.to_string();
                button.set_sensitive(true);
                button.connect_clicked(move |_| {
                    let on_action = on_action.clone();
                    let ts = ts.clone();
                    glib::idle_add_local_once(move || {
                        (on_action)(
                            crate::timeline_message_widget::TimelineAction::ExecuteControlAction {
                                ts,
                                key,
                            },
                        );
                    });
                });
            } else {
                button.set_sensitive(false);
            }
        } else {
            button.set_sensitive(false);
            button.set_tooltip_text(Some("Open this message in Slack to use this action"));
        }
        button
    }
}

/// A small square image (section accessory, quote avatar): the cached asset
/// when present, otherwise the context-icon placeholder at the same size.
fn context_sized_image(image: &MessageImage, sources: &MediaSources<'_>, size: i32) -> Widget {
    let widget = context_icon(image, sources);
    widget.set_size_request(size, size);
    if let Ok(picture) = widget.clone().downcast::<gtk::Picture>() {
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.add_css_class("timeline-media-image");
        picture.set_overflow(gtk::Overflow::Hidden);
    }
    widget
}

/// CSS class carrying an attachment's accent color as its left border.
/// `color` is pre-normalized to `#rgb`/`#rrggbb` by the document normalizer;
/// anything else is rejected rather than injected into CSS.
fn accent_class(color: &str) -> Option<String> {
    let hex = color.strip_prefix('#')?;
    if !matches!(hex.len(), 3 | 6) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let class = format!("attachment-accent-{}", hex.to_ascii_lowercase());
    let newly_seen = ACCENT_CLASSES.with(|seen| seen.borrow_mut().insert(class.clone()));
    if newly_seen && gtk::is_initialized_main_thread() {
        if let Some(display) = gtk::gdk::Display::default() {
            let provider = CssProvider::new();
            provider.load_from_string(&format!(
                ".{class} {{ border-left-color: #{}; }}",
                hex.to_ascii_lowercase()
            ));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
    }
    Some(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str, style: RichInlineStyle) -> RichInline {
        RichInline::Text {
            text: value.to_string(),
            style,
        }
    }

    #[test]
    fn inlines_become_escaped_mrkdwn_with_tight_style_markers() {
        let bold = RichInlineStyle {
            bold: true,
            ..RichInlineStyle::default()
        };
        let inlines = vec![
            text("Rich text ", bold),
            text("a < b & c", RichInlineStyle::default()),
            RichInline::Link {
                url: "https://example.com".to_string(),
                label: "click".to_string(),
                style: RichInlineStyle::default(),
            },
            RichInline::User("U123".to_string()),
            RichInline::Channel("C123".to_string()),
            RichInline::Emoji("wave".to_string()),
        ];

        assert_eq!(
            inlines_to_mrkdwn(&inlines),
            "*Rich text* a &lt; b &amp; c<https://example.com|click><@U123><#C123>:wave:"
        );
    }

    #[test]
    fn accent_class_rejects_values_that_are_not_hex_colors() {
        assert_eq!(
            accent_class("#2EB67D").as_deref(),
            Some("attachment-accent-2eb67d")
        );
        assert_eq!(accent_class("red; } * { color: red"), None);
        assert_eq!(accent_class("#12345"), None);
    }

    #[test]
    fn attachments_alone_do_not_replace_message_text() {
        let quote_only = MessageDocument::new(
            vec![MessageNode::Attachment(std::boxed::Box::new(
                MessageAttachment {
                    color: None,
                    pretext: None,
                    author: None,
                    title: None,
                    text: Some("Unfurl".to_string()),
                    fallback: None,
                    fields: Vec::new(),
                    image: None,
                    actions: Vec::new(),
                    footer: None,
                    footer_icon: None,
                },
            ))],
            None,
        );
        let image = MessageDocument::new(
            vec![MessageNode::Image(MessageImage::new(None, "GIF", None))],
            None,
        );

        assert!(!document_replaces_text(&quote_only));
        assert!(document_replaces_text(&image));
    }
}
