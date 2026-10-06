//! Card widgets for secondary view rows.

use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::pango;

use super::model::{FileRow, SavedRow, SearchRow, SecondaryAction, SecondaryRow, ThreadRow};
use crate::message_html::MessageHtmlContext;
use crate::timeline_message_widget::{
    build_timeline_message_widget, get_or_load_texture, resolve_cached_asset_path, TimelineAction,
};

pub(crate) type Dispatch = Rc<dyn Fn(SecondaryAction)>;

const CARD_MAXIMUM_WIDTH: i32 = 880;
const CARD_TIGHTENING_THRESHOLD: i32 = 600;
const FILE_THUMBNAIL_SIZE: i32 = 48;
const SNIPPET_LINES: i32 = 4;

fn register_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            r#"
            .secondary-card { padding: 12px; }
            .secondary-unread-badge {
                background-color: var(--accent-bg-color);
                color: var(--accent-fg-color);
                border-radius: 9999px;
                padding: 0 7px;
                min-width: 10px;
            }
            .secondary-thumbnail { border-radius: 6px; }
            .secondary-file-icon {
                border-radius: 6px;
                background-color: alpha(currentColor, 0.08);
            }
            "#,
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// Builds the clamped card for `row`. Buttons inside the card dispatch their
/// own actions; activating the list item runs [`SecondaryRow::primary_action`].
pub(crate) fn build_row(
    row: &SecondaryRow,
    context: &MessageHtmlContext,
    dispatch: &Dispatch,
) -> gtk::Widget {
    register_css();
    let card = match row {
        SecondaryRow::Thread(row) => thread_card(row, context, dispatch),
        SecondaryRow::Search(row) => search_card(row, dispatch),
        SecondaryRow::File(row) => file_card(row, context),
        SecondaryRow::Saved(row) => saved_card(row, context, dispatch),
    };
    let clamp = adw::Clamp::builder()
        .maximum_size(CARD_MAXIMUM_WIDTH)
        .tightening_threshold(CARD_TIGHTENING_THRESHOLD)
        .child(&card)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    clamp.upcast()
}

fn card_box() -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.add_css_class("card");
    card.add_css_class("secondary-card");
    card
}

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.add_css_class("caption-heading");
    label
}

fn timestamp_label(ts: &str) -> Option<gtk::Label> {
    let (_, full, short) = crate::message_html::localized_timestamp_parts(ts)?;
    let label = caption(&short);
    label.set_tooltip_text(Some(&full));
    Some(label)
}

fn icon_button(
    icon_name: &str,
    tooltip: &str,
    dispatch: &Dispatch,
    action: SecondaryAction,
) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    button.set_tooltip_text(Some(tooltip));
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
    let dispatch = dispatch.clone();
    button.connect_clicked(move |_| dispatch(action.clone()));
    button
}

fn author_callback(dispatch: &Dispatch) -> Rc<dyn Fn(TimelineAction)> {
    let dispatch = dispatch.clone();
    Rc::new(move |action| dispatch(SecondaryAction::Author(action)))
}

/// The message rendered by the shared timeline renderer, without hover
/// actions or clickable media: activating the card opens it in context.
/// Its replies button opens the thread.
fn message_widget(
    channel_id: &str,
    message: &crate::models::SlackMessage,
    context: &MessageHtmlContext,
    dispatch: &Dispatch,
) -> gtk::Widget {
    let author = author_callback(dispatch);
    let open_thread: Rc<dyn Fn(String)> = {
        let dispatch = dispatch.clone();
        let channel_id = channel_id.to_string();
        Rc::new(move |thread_ts| {
            dispatch(SecondaryAction::OpenThread {
                channel_id: channel_id.clone(),
                thread_ts,
            })
        })
    };
    build_timeline_message_widget(
        message,
        context,
        None,
        Some(&open_thread),
        None,
        None,
        Some(&author),
    )
    .upcast()
}

fn thread_card(row: &ThreadRow, context: &MessageHtmlContext, dispatch: &Dispatch) -> gtk::Box {
    let card = card_box();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let channel = heading(&row.channel_title);
    channel.set_hexpand(true);
    header.append(&channel);
    if row.unread {
        let count = if row.unread_count > 0 {
            row.unread_count.to_string()
        } else {
            String::new()
        };
        let badge = gtk::Label::new(Some(&count));
        badge.add_css_class("secondary-unread-badge");
        badge.add_css_class("caption-heading");
        badge.set_valign(gtk::Align::Center);
        badge.set_tooltip_text(Some(&gettext("Unread replies")));
        header.append(&badge);
    }
    card.append(&header);
    card.append(&message_widget(
        &row.channel_id,
        &row.root,
        context,
        dispatch,
    ));
    if !row.participants.is_empty() {
        let participants = caption(&row.participants);
        participants.set_tooltip_text(Some(&gettext("Participants")));
        card.append(&participants);
    }
    card
}

fn search_card(row: &SearchRow, dispatch: &Dispatch) -> gtk::Box {
    let card = card_box();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let author = heading(&row.author);
    header.append(&author);
    let channel = caption(&row.channel_title);
    channel.set_hexpand(true);
    header.append(&channel);
    if let Some(time) = row.ts.as_deref().and_then(timestamp_label) {
        header.append(&time);
    }
    if let Some(permalink) = row.permalink.clone() {
        header.append(&icon_button(
            "adw-external-link-symbolic",
            &gettext("Open in Slack"),
            dispatch,
            SecondaryAction::OpenExternal(permalink),
        ));
    }
    card.append(&header);

    let snippet = gtk::Label::new(None);
    snippet.set_markup(&row.snippet_markup);
    snippet.set_xalign(0.0);
    snippet.set_wrap(true);
    snippet.set_wrap_mode(pango::WrapMode::WordChar);
    snippet.set_lines(SNIPPET_LINES);
    snippet.set_ellipsize(pango::EllipsizeMode::End);
    snippet.add_css_class("body");
    card.append(&snippet);
    card
}

fn file_thumbnail(row: &FileRow, context: &MessageHtmlContext) -> gtk::Widget {
    let texture = row
        .thumbnail_url
        .as_deref()
        .and_then(|url| resolve_cached_asset_path(url, context))
        .and_then(|path| get_or_load_texture(&path));
    if let Some(texture) = texture {
        let picture = gtk::Picture::for_paintable(&texture);
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_size_request(FILE_THUMBNAIL_SIZE, FILE_THUMBNAIL_SIZE);
        picture.set_overflow(gtk::Overflow::Hidden);
        picture.add_css_class("secondary-thumbnail");
        return picture.upcast();
    }
    let icon = gtk::Image::from_icon_name(row.icon_name);
    icon.set_pixel_size(24);
    icon.set_size_request(FILE_THUMBNAIL_SIZE, FILE_THUMBNAIL_SIZE);
    icon.add_css_class("secondary-file-icon");
    icon.upcast()
}

fn file_meta_text(row: &FileRow) -> String {
    let date = row
        .created_ts
        .as_deref()
        .and_then(crate::message_html::localized_timestamp_parts)
        .map(|(_, _, short)| short);
    [
        Some(row.detail.clone()),
        row.owner.clone(),
        row.channel_title.clone(),
        date,
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" \u{b7} ")
}

fn file_card(row: &FileRow, context: &MessageHtmlContext) -> gtk::Box {
    let card = card_box();
    card.set_orientation(gtk::Orientation::Horizontal);
    card.set_spacing(12);
    let thumbnail = file_thumbnail(row, context);
    thumbnail.set_valign(gtk::Align::Center);
    card.append(&thumbnail);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    let title = gtk::Label::new(Some(&row.title));
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::Middle);
    title.add_css_class("heading");
    text.append(&title);
    let meta = file_meta_text(row);
    if !meta.is_empty() {
        text.append(&caption(&meta));
    }
    card.append(&text);
    card
}

fn saved_card(row: &SavedRow, context: &MessageHtmlContext, dispatch: &Dispatch) -> gtk::Box {
    let card = card_box();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let channel = heading(&row.channel_title);
    channel.set_hexpand(true);
    header.append(&channel);
    header.append(&icon_button(
        "object-select-symbolic",
        &gettext("Mark Complete"),
        dispatch,
        SecondaryAction::RemoveSaved {
            channel_id: row.channel_id.clone(),
            ts: row.message.ts.clone(),
            thread_ts: row.message.thread_ts.clone(),
        },
    ));
    card.append(&header);
    card.append(&message_widget(
        &row.channel_id,
        &row.message,
        context,
        dispatch,
    ));
    card
}
