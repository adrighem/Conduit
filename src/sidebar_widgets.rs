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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::sidebar::{
    KeyedSidebarItem, SidebarItemKey, SidebarItemModel, SidebarProjectionChange,
    SidebarProjectionOperation, SidebarRowModel,
};

mod imp {
    use super::*;
    use glib::subclass::prelude::*;

    #[derive(Debug, Default)]
    pub struct SidebarListItemObject {
        pub item: RefCell<Option<KeyedSidebarItem>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SidebarListItemObject {
        const NAME: &'static str = "ConduitSidebarListItemObject";
        type Type = super::SidebarListItemObject;
    }

    impl ObjectImpl for SidebarListItemObject {}
}

glib::wrapper! {
    pub struct SidebarListItemObject(ObjectSubclass<imp::SidebarListItemObject>);
}

impl SidebarListItemObject {
    pub fn new(item: KeyedSidebarItem) -> Self {
        use glib::subclass::prelude::ObjectSubclassIsExt;

        let object: Self = glib::Object::new();
        object.imp().item.replace(Some(item));
        object
    }

    pub fn item(&self) -> KeyedSidebarItem {
        use glib::subclass::prelude::ObjectSubclassIsExt;

        self.imp()
            .item
            .borrow()
            .as_ref()
            .expect("sidebar list item objects are initialized at construction")
            .clone()
    }

    pub fn replace_model(&self, key: &SidebarItemKey, model: SidebarItemModel) -> bool {
        use glib::subclass::prelude::ObjectSubclassIsExt;

        let mut item = self.imp().item.borrow_mut();
        let item = item
            .as_mut()
            .expect("sidebar list item objects are initialized at construction");
        if &item.key != key || item.model == model {
            return false;
        }
        item.model = model;
        true
    }
}

#[derive(Debug)]
pub struct SidebarListStore {
    model: gio::ListStore,
    objects: HashMap<SidebarItemKey, SidebarListItemObject>,
}

impl Default for SidebarListStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SidebarListStore {
    pub fn new() -> Self {
        Self {
            model: gio::ListStore::new::<SidebarListItemObject>(),
            objects: HashMap::new(),
        }
    }

    pub fn model(&self) -> gio::ListStore {
        self.model.clone()
    }

    pub fn n_items(&self) -> u32 {
        self.model.n_items()
    }

    pub fn object_at(&self, position: u32) -> Option<SidebarListItemObject> {
        self.model
            .item(position)
            .and_then(|item| item.downcast::<SidebarListItemObject>().ok())
    }

    pub fn item_at(&self, position: u32) -> Option<KeyedSidebarItem> {
        self.object_at(position).map(|object| object.item())
    }

    pub fn apply(&mut self, change: &SidebarProjectionChange, final_items: &[KeyedSidebarItem]) {
        for operation in &change.operations {
            match operation {
                SidebarProjectionOperation::Reset { items } => {
                    self.objects.clear();
                    let objects = items
                        .iter()
                        .cloned()
                        .map(|item| {
                            let key = item.key.clone();
                            let object = SidebarListItemObject::new(item);
                            self.objects.insert(key, object.clone());
                            object
                        })
                        .collect::<Vec<_>>();
                    self.model.splice(0, self.model.n_items(), &objects);
                }
                SidebarProjectionOperation::Splice {
                    position,
                    removed,
                    inserted,
                } => {
                    self.model.splice(
                        *position,
                        removed.len() as u32,
                        &[] as &[SidebarListItemObject],
                    );
                    let objects = inserted
                        .iter()
                        .cloned()
                        .map(|item| {
                            let key = item.key.clone();
                            let object = self
                                .objects
                                .entry(key)
                                .or_insert_with(|| SidebarListItemObject::new(item.clone()))
                                .clone();
                            object.replace_model(&item.key, item.model);
                            object
                        })
                        .collect::<Vec<_>>();
                    if !objects.is_empty() {
                        self.model.splice(*position, 0, &objects);
                    }
                }
                SidebarProjectionOperation::Update {
                    position,
                    key,
                    model,
                } => {
                    let object = self
                        .objects
                        .get(key)
                        .expect("projection updates refer to an existing sidebar key")
                        .clone();
                    object.replace_model(key, model.clone());
                    self.model
                        .splice(*position, 1, std::slice::from_ref(&object));
                }
            }
        }

        let retained = final_items
            .iter()
            .map(|item| &item.key)
            .collect::<HashSet<_>>();
        self.objects.retain(|key, _| retained.contains(key));

        debug_assert_eq!(self.n_items() as usize, final_items.len());
        debug_assert!(final_items
            .iter()
            .enumerate()
            .all(|(position, item)| self.item_at(position as u32).as_ref() == Some(item)));
    }
}

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

    if model.starred {
        let starred = gtk::Image::from_icon_name("starred-symbolic");
        starred.set_tooltip_text(Some("Starred"));
        starred.update_property(&[gtk::accessible::Property::Label("Starred")]);
        content.append(&starred);
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
    use crate::sidebar::{
        ConversationKind, KeyedSidebarItem, SidebarItemKey, SidebarItemModel, SidebarPlaceholder,
        SidebarProjection, SidebarRowModel, SidebarSectionKind,
    };

    fn conversation_item(id: &str) -> KeyedSidebarItem {
        KeyedSidebarItem {
            key: SidebarItemKey::Conversation {
                section: None,
                id: id.to_string(),
            },
            model: SidebarItemModel::Conversation(SidebarRowModel {
                id: id.to_string(),
                title: id.to_string(),
                kind: ConversationKind::PublicChannel,
                unread: false,
                unread_count: 0,
                selected: false,
                starred: false,
                private: false,
                muted: false,
                external: false,
                huddle_active: false,
                search_aliases: Vec::new(),
                status: None,
            }),
        }
    }

    #[test]
    fn title_weight_uses_bold_only_for_unread_rows() {
        assert_eq!(sidebar_title_weight(false), gtk::pango::Weight::Normal);
        assert_eq!(sidebar_title_weight(true), gtk::pango::Weight::Bold);
    }

    #[test]
    fn sidebar_list_item_object_updates_content_without_changing_identity() {
        let object = SidebarListItemObject::new(KeyedSidebarItem {
            key: SidebarItemKey::Placeholder,
            model: SidebarItemModel::Placeholder(SidebarPlaceholder::Loading),
        });
        let same_object = object.clone();

        assert!(object.replace_model(
            &SidebarItemKey::Placeholder,
            SidebarItemModel::Placeholder(SidebarPlaceholder::LoadFailed),
        ));
        assert_eq!(object, same_object);
        assert_eq!(object.item().key, SidebarItemKey::Placeholder);
        assert_eq!(
            object.item().model,
            SidebarItemModel::Placeholder(SidebarPlaceholder::LoadFailed)
        );
        assert!(!object.replace_model(
            &SidebarItemKey::SectionHeader(SidebarSectionKind::Channels),
            SidebarItemModel::SectionHeader {
                kind: SidebarSectionKind::Channels,
                title: "Channels".to_string(),
                collapsed: false,
            },
        ));
        assert_eq!(object.item().key, SidebarItemKey::Placeholder);
    }

    #[test]
    fn sidebar_list_store_updates_one_of_1430_items_without_replacing_objects() {
        let initial = (0..1_430)
            .map(|index| conversation_item(&format!("C{index:04}")))
            .collect::<Vec<_>>();
        let mut projection = SidebarProjection::default();
        let reset = projection.reset(initial.clone()).unwrap();
        let mut store = SidebarListStore::new();
        store.apply(&reset, &initial);
        let target_position = 715;
        let target_before = store.object_at(target_position).unwrap();
        let neighbor_before = store.object_at(target_position + 1).unwrap();
        let mut next = initial;
        let SidebarItemModel::Conversation(row) = &mut next[target_position as usize].model else {
            panic!("expected conversation row");
        };
        row.unread = true;
        row.unread_count = 1;

        let change = projection.reconcile(next.clone()).unwrap();
        store.apply(&change, &next);

        assert_eq!(store.n_items(), 1_430);
        assert_eq!(store.object_at(target_position).unwrap(), target_before);
        assert_eq!(
            store.object_at(target_position + 1).unwrap(),
            neighbor_before
        );
        assert_eq!(
            store.item_at(target_position).unwrap(),
            next[target_position as usize]
        );
    }

    #[test]
    fn sidebar_list_store_reuses_keyed_objects_across_moves() {
        let initial = vec![
            conversation_item("C1"),
            conversation_item("C2"),
            conversation_item("C3"),
        ];
        let mut projection = SidebarProjection::default();
        let reset = projection.reset(initial.clone()).unwrap();
        let mut store = SidebarListStore::new();
        store.apply(&reset, &initial);
        let moved = store.object_at(2).unwrap();
        let next = vec![initial[2].clone(), initial[0].clone(), initial[1].clone()];

        let change = projection.reconcile(next.clone()).unwrap();
        store.apply(&change, &next);

        assert_eq!(store.object_at(0).unwrap(), moved);
        assert_eq!(
            (0..store.n_items())
                .map(|position| store.item_at(position).unwrap().key)
                .collect::<Vec<_>>(),
            next.into_iter().map(|item| item.key).collect::<Vec<_>>()
        );
    }
}
