/* sidebar_widgets.rs
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

use gtk::prelude::*;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::sidebar::SidebarRowModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarRowLayout {
    margin_top: i32,
    margin_bottom: i32,
    margin_start: i32,
    margin_end: i32,
    show_avatars: bool,
}

impl SidebarRowLayout {
    pub fn sidebar() -> Self {
        Self {
            // Spacing comes from CSS padding (`.conduit-sidebar-list`), so the
            // open-conversation highlight fills the row like the hover one.
            margin_top: 0,
            margin_bottom: 0,
            margin_start: 0,
            margin_end: 0,
            show_avatars: false,
        }
    }

    pub fn switcher() -> Self {
        Self {
            margin_top: 6,
            margin_bottom: 6,
            margin_start: 8,
            margin_end: 8,
            show_avatars: true,
        }
    }
}

pub fn sidebar_row_widget(
    model: &SidebarRowModel,
    layout: SidebarRowLayout,
    custom_emojis: &std::collections::HashMap<String, String>,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(true);
    row.set_activatable(true);
    if model.selected {
        row.add_css_class("selected");
    }
    let accessible_label = model.accessible_label();
    row.set_tooltip_text(Some(&accessible_label));
    row.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.add_css_class("sidebar-row-content");
    if model.selected {
        content.add_css_class("active-conversation");
    }
    content.set_margin_top(layout.margin_top);
    content.set_margin_bottom(layout.margin_bottom);
    content.set_margin_start(layout.margin_start);
    content.set_margin_end(layout.margin_end);

    if layout.show_avatars {
        if let Some(avatar_url) = model.avatar_url.as_deref() {
            register_switcher_avatar_css();
            let avatar = switcher_avatar_picture(avatar_url, model.kind.accessible_name(), 20);
            content.append(&avatar);
        } else {
            let icon = gtk::Image::from_icon_name(model.kind.icon_name());
            icon.set_tooltip_text(Some(model.kind.accessible_name()));
            content.append(&icon);
        }
    } else {
        let icon = gtk::Image::from_icon_name(model.kind.icon_name());
        icon.set_tooltip_text(Some(model.kind.accessible_name()));
        content.append(&icon);
    }

    let emphasized = model.unread || model.mention_count > 0;
    let title = gtk::Label::new(Some(&model.title));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_attributes(Some(&sidebar_title_attributes(emphasized)));
    if emphasized {
        title.add_css_class("heading");
    }

    // `title` deliberately does NOT hexpand here: the status indicator sits
    // right next to the visible text. Hexpand instead applies to this
    // wrapping row, so the unused space ends up before trailing indicators
    // and the mention pill stays right-aligned at the row end.
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    title_row.set_hexpand(true);
    title_row.append(&title);

    content.append(&title_row);

    if let Some(status) = model.status.as_ref() {
        let text = status.accessible_text();
        let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        let emoji_size = sidebar_target_emoji_size(&title);

        match EmojiCatalog::new(custom_emojis).resolve(status.emoji_name()) {
            Some(EmojiValue::Unicode(glyph)) => {
                let label = gtk::Label::new(Some(glyph));
                label.add_css_class("status-emoji");
                indicator.append(&label);
            }
            Some(EmojiValue::CustomImage(url)) => {
                let picture = sidebar_status_emoji_picture(&url, &text, emoji_size);
                picture.add_css_class("status-emoji");
                indicator.append(&picture);
            }
            None => {
                let label = gtk::Label::new(Some("●"));
                label.add_css_class("status-emoji");
                indicator.append(&label);
            }
        }
        indicator.set_focusable(true);
        indicator.set_tooltip_text(Some(&text));
        indicator.update_property(&[gtk::accessible::Property::Label(&format!("Status: {text}"))]);
        content.append(&indicator);
    }

    if model.starred {
        let starred = gtk::Image::from_icon_name("starred-symbolic");
        starred.set_tooltip_text(Some("Starred"));
        starred.update_property(&[gtk::accessible::Property::Label("Starred")]);
        content.append(&starred);
    }

    if model.muted {
        let muted = gtk::Image::from_icon_name("notifications-disabled-symbolic");
        muted.set_tooltip_text(Some("Muted"));
        content.append(&muted);
    }

    if model.external {
        let external = gtk::Image::from_icon_name("network-workgroup-symbolic");
        external.set_tooltip_text(Some("Shared externally"));
        content.append(&external);
    }

    if model.huddle_active {
        let huddle = gtk::Image::from_icon_name("call-start-symbolic");
        huddle.set_tooltip_text(Some("Huddle active"));
        huddle.update_property(&[gtk::accessible::Property::Label("Huddle active")]);
        content.append(&huddle);
    }

    if let Some(mention_label) = model.mention_badge_label() {
        let mention = gtk::Label::new(Some(&mention_label));
        mention.add_css_class("mention-badge-pill");
        mention.set_valign(gtk::Align::Center);
        mention.set_tooltip_text(Some("Unread mentions"));
        content.append(&mention);
    }

    row.set_child(Some(&content));
    row
}

fn sidebar_target_emoji_size(widget: &impl gtk::prelude::WidgetExt) -> i32 {
    let context = widget.pango_context();
    let metrics = context.metrics(None, None);
    let height = (metrics.ascent() + metrics.descent()) / gtk::pango::SCALE;
    if height > 0 {
        height.clamp(12, 48)
    } else {
        16
    }
}

fn register_switcher_avatar_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !gtk::is_initialized() {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_string(".switcher-avatar { border-radius: 9999px; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

fn switcher_avatar_picture(url: &str, label: &str, size: i32) -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_alternative_text(Some(label));
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_size_request(size, size);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);
    picture.set_overflow(gtk::Overflow::Hidden);
    picture.add_css_class("switcher-avatar");

    load_sidebar_image(&picture, url, size);
    picture
}

fn sidebar_status_emoji_picture(url: &str, label: &str, size: i32) -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_alternative_text(Some(label));
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_size_request(size, size);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);

    load_sidebar_image(&picture, url, size);
    picture
}

/// Shows the image at `url` in `picture`, scaled to `size`. Only cached
/// downloads are decoded, never the raw URL: the shared emoji loader rejects
/// non-https and private hosts and caps the download size, and the decode
/// budget rejects files that would inflate to gigabytes of pixels.
fn load_sidebar_image(picture: &gtk::Picture, url: &str, size: i32) {
    let weak_picture = picture.downgrade();
    if let Some(path) = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::emoji_picker_window::resolve_custom_emoji_cached_file(url))
    {
        decode_sidebar_image(weak_picture, &path, size);
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();
    crate::emoji_picker_window::request_custom_emoji_file(url, move |path| {
        let _ = tx.send(path);
    });
    gtk::glib::spawn_future_local(async move {
        if let Some(path) = rx.recv().await {
            decode_sidebar_image(weak_picture, &path, size);
        }
    });
}

fn decode_sidebar_image(
    weak_picture: gtk::glib::WeakRef<gtk::Picture>,
    path: &std::path::Path,
    size: i32,
) {
    if weak_picture.upgrade().is_none() || !crate::animated_image::within_decode_budget(path) {
        return;
    }
    gtk::gio::File::for_path(path).read_async(
        gtk::glib::Priority::DEFAULT,
        gtk::gio::Cancellable::NONE,
        move |stream| {
            let Ok(stream) = stream else { return };
            gdk_pixbuf::PixbufAnimation::from_stream_async(
                &stream,
                gtk::gio::Cancellable::NONE,
                move |animation| {
                    let Some(picture) = weak_picture.upgrade() else {
                        return;
                    };
                    let Ok(animation) = animation else { return };
                    let frame = animation.iter(Some(std::time::SystemTime::now()));
                    let pixbuf = frame.pixbuf();
                    let w = pixbuf.width();
                    let h = pixbuf.height();
                    let (target_w, target_h) = if w > h {
                        (size, (size * h / w).max(1))
                    } else {
                        ((size * w / h).max(1), size)
                    };
                    let scaled = pixbuf
                        .scale_simple(target_w, target_h, gdk_pixbuf::InterpType::Bilinear)
                        .unwrap_or(pixbuf);
                    picture.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&scaled)));
                },
            );
        },
    );
}

fn sidebar_title_attributes(unread: bool) -> gtk::pango::AttrList {
    let attributes = gtk::pango::AttrList::new();
    attributes.insert(gtk::pango::AttrInt::new_weight(sidebar_title_weight(
        unread,
    )));
    attributes
}

fn sidebar_title_weight(unread: bool) -> gtk::pango::Weight {
    if unread {
        gtk::pango::Weight::Bold
    } else {
        gtk::pango::Weight::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_weight_uses_bold_only_for_unread_rows() {
        assert_eq!(sidebar_title_weight(false), gtk::pango::Weight::Normal);
        assert_eq!(sidebar_title_weight(true), gtk::pango::Weight::Bold);
    }
}
