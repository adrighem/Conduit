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
}

impl SidebarRowLayout {
    pub fn sidebar() -> Self {
        Self {
            margin_top: 1,
            margin_bottom: 1,
            margin_start: 6,
            margin_end: 6,
        }
    }

    pub fn switcher() -> Self {
        Self {
            margin_top: 6,
            margin_bottom: 6,
            margin_start: 8,
            margin_end: 8,
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
    let accessible_label = model.accessible_label();
    row.set_tooltip_text(Some(&accessible_label));
    row.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_margin_top(layout.margin_top);
    content.set_margin_bottom(layout.margin_bottom);
    content.set_margin_start(layout.margin_start);
    content.set_margin_end(layout.margin_end);

    let icon = gtk::Image::from_icon_name(model.kind.icon_name());
    icon.set_tooltip_text(Some(model.kind.accessible_name()));
    content.append(&icon);

    let title = gtk::Label::new(Some(&model.title));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_attributes(Some(&sidebar_title_attributes(model.unread)));
    if model.unread {
        title.add_css_class("heading");
    }
    content.append(&title);

    if let Some(status) = model.status.as_ref() {
        let text = status.accessible_text();
        let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        match EmojiCatalog::new(custom_emojis).resolve(status.emoji_name()) {
            Some(EmojiValue::Unicode(glyph)) => {
                indicator.append(&gtk::Label::new(Some(glyph)));
            }
            Some(EmojiValue::CustomImage(url)) => {
                let picture = gtk::Picture::for_file(&gtk::gio::File::for_uri(&url));
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.set_width_request(16);
                picture.set_height_request(16);
                indicator.append(&picture);
            }
            None => indicator.append(&gtk::Label::new(Some("●"))),
        }
        indicator.set_focusable(true);
        indicator.set_tooltip_text(Some(&text));
        indicator.update_property(&[gtk::accessible::Property::Label(&format!("Status: {text}"))]);
        content.append(&indicator);
    }

    if let Some(unread_label) = model.unread_badge_label() {
        let unread = gtk::Label::new(Some(&unread_label));
        unread.add_css_class("caption");
        unread.add_css_class("heading");
        content.append(&unread);
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

    row.set_child(Some(&content));
    row
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

    #[test]
    fn sidebar_rows_are_denser_than_switcher_rows() {
        let sidebar = SidebarRowLayout::sidebar();
        let switcher = SidebarRowLayout::switcher();

        assert!(sidebar.margin_top < switcher.margin_top);
        assert!(sidebar.margin_bottom < switcher.margin_bottom);
        assert_eq!(sidebar.margin_top, 1);
        assert_eq!(sidebar.margin_bottom, 1);
    }
}
