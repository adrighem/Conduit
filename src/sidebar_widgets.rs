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

use crate::sidebar::{KeyedSidebarItem, SidebarItemModel};
use crate::window::{sidebar_context_menu_key, sidebar_section_accessible_label};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    pub(crate) static SIDEBAR_SKELETON_BUILDS: Cell<usize> = const { Cell::new(0) };
    pub(crate) static SIDEBAR_ROW_UPDATES: Cell<usize> = const { Cell::new(0) };
    pub(crate) static SIDEBAR_WIDGET_CONSTRUCTIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn count_widget_construction() {
    SIDEBAR_WIDGET_CONSTRUCTIONS.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
pub(crate) fn count_widget_construction() {}

use gtk::prelude::*;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::sidebar::SidebarRowModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SidebarRowLayout {
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

pub(crate) fn register_switcher_avatar_css() {
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

/// Shows the image at `url` in `picture`, scaled to `size`. Only cached
/// downloads are decoded, never the raw URL: the shared emoji loader rejects
/// non-https and private hosts and caps the download size, and the decode
/// budget rejects files that would inflate to gigabytes of pixels.
/// Most distinct custom status emoji (per size) kept decoded. Past this the
/// cache starts over, which only costs one decode per visible emoji.
const SIDEBAR_TEXTURE_CACHE_LIMIT: usize = 64;

type SidebarTextureKey = (String, i32);

thread_local! {
    /// Decoded status emoji by URL and size, so a rebuilt row does not stat,
    /// probe and decode the same file again.
    static SIDEBAR_TEXTURES: std::cell::RefCell<
        std::collections::HashMap<SidebarTextureKey, gtk::gdk::Texture>,
    > = std::cell::RefCell::default();
}

fn cached_sidebar_texture(key: &SidebarTextureKey) -> Option<gtk::gdk::Texture> {
    SIDEBAR_TEXTURES.with(|cache| cache.borrow().get(key).cloned())
}

fn store_sidebar_texture(key: SidebarTextureKey, texture: &gtk::gdk::Texture) {
    SIDEBAR_TEXTURES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= SIDEBAR_TEXTURE_CACHE_LIMIT && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, texture.clone());
    });
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

    mod graphical {
        use super::*;
        use crate::timeline::tests::run_gtk_test;

        #[test]
        fn skeleton_context_menu_cleans_up_on_reopen_and_popdown() {
            run_gtk_test(|| {
                use crate::timeline::tests::{pump, pump_until};

                let opened: Rc<RefCell<Vec<(String, gtk::Popover)>>> = Rc::default();
                let opened_sink = Rc::clone(&opened);
                let handler: SidebarContextMenuHandler =
                    Rc::new(move |row, channel_id, pointing_to| {
                        let popover = gtk::Popover::new();
                        popover.set_parent(row);
                        popover.set_pointing_to(pointing_to.as_ref());
                        popover.set_child(Some(&gtk::Button::with_label("Menu")));
                        popover.popup();
                        opened_sink
                            .borrow_mut()
                            .push((channel_id.to_string(), popover.clone()));
                        Some(popover)
                    });

                let skeleton = SidebarItemSkeleton::new(&handler);
                let window = gtk::Window::builder()
                    .default_width(240)
                    .default_height(120)
                    .child(&skeleton.conversation.content)
                    .build();
                window.present();
                pump_until(5000, || window.height() > 0);

                // Unbound skeleton ignores context menu requests.
                assert!(!skeleton.open_context_menu(
                    &handler,
                    &skeleton.conversation.content,
                    gtk::gdk::Rectangle::new(4, 4, 1, 1),
                ));
                assert!(opened.borrow().is_empty());

                // Opening twice pops down and unparents the first popover without leaking.
                *skeleton.bound_channel_id.borrow_mut() = Some("C_FIRST".to_string());
                assert!(skeleton.open_context_menu(
                    &handler,
                    &skeleton.conversation.content,
                    gtk::gdk::Rectangle::new(4, 4, 1, 1),
                ));
                pump(20);
                *skeleton.bound_channel_id.borrow_mut() = Some("C_SECOND".to_string());
                assert!(skeleton.open_context_menu(
                    &handler,
                    &skeleton.conversation.content,
                    gtk::gdk::Rectangle::new(8, 8, 1, 1),
                ));

                let entries = opened.borrow().clone();
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].0, "C_FIRST");
                assert_eq!(entries[1].0, "C_SECOND");
                assert!(
                    entries[0].1.parent().is_none(),
                    "first popover must be unparented when second menu opens"
                );
                assert!(entries[1].1.parent().is_some());

                // Popdown (used by unbind and teardown) unparents the active popover.
                skeleton.popdown_context_menu();
                assert!(
                    entries[1].1.parent().is_none(),
                    "active popover must be unparented on popdown_context_menu"
                );

                window.destroy();
            });
        }

        #[test]
        fn perf_guard_sidebar_skeleton_does_not_leak() {
            run_gtk_test(|| {
                let handler: SidebarContextMenuHandler = Rc::new(|_, _, _| None);
                let (weak_skeleton, weak_content) = {
                    let skeleton = SidebarItemSkeleton::new(&handler);
                    (
                        Rc::downgrade(&skeleton),
                        skeleton.conversation.content.downgrade(),
                    )
                };
                assert!(
                    weak_skeleton.upgrade().is_none(),
                    "SidebarItemSkeleton leaked via strong controller closure cycle"
                );
                assert!(
                    weak_content.upgrade().is_none(),
                    "ConversationRowWidgets content box leaked after skeleton drop"
                );
            });
        }
    }
}

pub(crate) struct ConversationRowWidgets {
    pub content: gtk::Box,
    pub avatar_picture: Option<gtk::Picture>,
    pub icon: gtk::Image,
    pub title: gtk::Label,
    pub status_indicator: gtk::Box,
    pub status_label: gtk::Label,
    pub status_picture: gtk::Picture,
    pub starred: gtk::Image,
    pub muted: gtk::Image,
    pub external: gtk::Image,
    pub huddle: gtk::Image,
    pub mention: gtk::Label,
    pub layout: SidebarRowLayout,
}

impl ConversationRowWidgets {
    pub fn new(layout: SidebarRowLayout) -> Self {
        count_widget_construction();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.add_css_class("sidebar-row-content");
        content.set_margin_top(layout.margin_top);
        content.set_margin_bottom(layout.margin_bottom);
        content.set_margin_start(layout.margin_start);
        content.set_margin_end(layout.margin_end);

        let avatar_picture = if layout.show_avatars {
            crate::sidebar_widgets::register_switcher_avatar_css();
            count_widget_construction();
            let pic = gtk::Picture::new();
            pic.add_css_class("switcher-avatar");
            pic.set_can_shrink(true);
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_size_request(20, 20);
            pic.set_halign(gtk::Align::Center);
            pic.set_valign(gtk::Align::Center);
            pic.set_overflow(gtk::Overflow::Hidden);
            content.append(&pic);
            Some(pic)
        } else {
            None
        };

        count_widget_construction();
        let icon = gtk::Image::new();

        count_widget_construction();
        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        title_row.set_hexpand(true);

        count_widget_construction();
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        title_row.append(&title);

        count_widget_construction();
        let status_indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        status_indicator.set_focusable(true);
        status_indicator.set_visible(false);

        count_widget_construction();
        let status_label = gtk::Label::new(None);
        status_label.add_css_class("status-emoji");
        status_label.set_visible(false);

        count_widget_construction();
        let status_picture = gtk::Picture::new();
        status_picture.add_css_class("status-emoji");
        status_picture.set_can_shrink(true);
        status_picture.set_content_fit(gtk::ContentFit::Contain);
        status_picture.set_halign(gtk::Align::Center);
        status_picture.set_valign(gtk::Align::Center);
        status_picture.set_visible(false);

        status_indicator.append(&status_label);
        status_indicator.append(&status_picture);

        count_widget_construction();
        let starred = gtk::Image::from_icon_name("starred-symbolic");
        starred.set_tooltip_text(Some("Starred"));
        starred.update_property(&[gtk::accessible::Property::Label("Starred")]);
        starred.set_visible(false);

        count_widget_construction();
        let muted = gtk::Image::from_icon_name("notifications-disabled-symbolic");
        muted.set_tooltip_text(Some("Muted"));
        muted.set_visible(false);

        count_widget_construction();
        let external = gtk::Image::from_icon_name("network-workgroup-symbolic");
        external.set_tooltip_text(Some("Shared externally"));
        external.set_visible(false);

        count_widget_construction();
        let huddle = gtk::Image::from_icon_name("call-start-symbolic");
        huddle.set_tooltip_text(Some("Huddle active"));
        huddle.update_property(&[gtk::accessible::Property::Label("Huddle active")]);
        huddle.set_visible(false);

        count_widget_construction();
        let mention = gtk::Label::new(None);
        mention.add_css_class("mention-badge-pill");
        mention.set_halign(gtk::Align::Center);
        mention.set_valign(gtk::Align::Center);
        mention.set_tooltip_text(Some("Unread mentions"));
        mention.set_visible(false);

        content.append(&icon);
        content.append(&title_row);
        content.append(&status_indicator);
        content.append(&starred);
        content.append(&muted);
        content.append(&external);
        content.append(&huddle);
        content.append(&mention);

        Self {
            content,
            avatar_picture,
            icon,
            title,
            status_indicator,
            status_label,
            status_picture,
            starred,
            muted,
            external,
            huddle,
            mention,
            layout,
        }
    }

    pub fn update(
        &self,
        model: &SidebarRowModel,
        custom_emojis: &std::collections::HashMap<String, String>,
        status_generation: Rc<Cell<u64>>,
        current_gen: u64,
    ) {
        #[cfg(test)]
        SIDEBAR_ROW_UPDATES.with(|c| c.set(c.get() + 1));

        if model.selected {
            self.content.add_css_class("active-conversation");
        } else {
            self.content.remove_css_class("active-conversation");
        }

        let accessible_label = model.accessible_label();
        self.content.set_tooltip_text(Some(&accessible_label));
        self.content
            .update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

        if self.layout.show_avatars {
            if let Some(avatar_url) = model.avatar_url.as_deref() {
                if let Some(pic) = &self.avatar_picture {
                    pic.set_visible(true);
                    load_sidebar_image_guarded(
                        pic,
                        avatar_url,
                        20,
                        status_generation.clone(),
                        current_gen,
                    );
                    pic.set_alternative_text(Some(model.kind.accessible_name()));
                }
                self.icon.set_visible(false);
            } else {
                if let Some(pic) = &self.avatar_picture {
                    pic.set_visible(false);
                    pic.set_paintable(None::<&gtk::gdk::Paintable>);
                }
                self.icon.set_icon_name(Some(model.kind.icon_name()));
                self.icon
                    .set_tooltip_text(Some(model.kind.accessible_name()));
                self.icon.set_visible(true);
            }
        } else {
            self.icon.set_icon_name(Some(model.kind.icon_name()));
            self.icon
                .set_tooltip_text(Some(model.kind.accessible_name()));
            self.icon.set_visible(true);
        }

        let emphasized = model.unread || model.mention_count > 0;
        self.title.set_text(&model.title);
        self.title
            .set_attributes(Some(&sidebar_title_attributes(emphasized)));
        if emphasized {
            self.title.add_css_class("heading");
        } else {
            self.title.remove_css_class("heading");
        }

        if let Some(status) = model.status.as_ref() {
            self.status_indicator.set_visible(true);
            let text = status.accessible_text();
            let accessible_status_label = format!("Status: {}", text);
            self.status_indicator.set_tooltip_text(Some(&text));
            self.status_indicator
                .update_property(&[gtk::accessible::Property::Label(&accessible_status_label)]);

            match EmojiCatalog::new(custom_emojis).resolve(status.emoji_name()) {
                Some(EmojiValue::Unicode(glyph)) => {
                    self.status_picture.set_visible(false);
                    self.status_picture
                        .set_paintable(None::<&gtk::gdk::Paintable>);
                    self.status_label.set_text(glyph);
                    self.status_label.set_visible(true);
                }
                Some(EmojiValue::CustomImage(url)) => {
                    self.status_label.set_visible(false);
                    self.status_picture.set_alternative_text(Some(&text));
                    let emoji_size = sidebar_target_emoji_size(&self.title);
                    self.status_picture.set_size_request(emoji_size, emoji_size);
                    self.status_picture.set_visible(true);

                    load_sidebar_image_guarded(
                        &self.status_picture,
                        &url,
                        emoji_size,
                        status_generation.clone(),
                        current_gen,
                    );
                }
                None => {
                    self.status_picture.set_visible(false);
                    self.status_picture
                        .set_paintable(None::<&gtk::gdk::Paintable>);
                    self.status_label.set_text("●");
                    self.status_label.set_visible(true);
                }
            }
        } else {
            self.status_indicator.set_visible(false);
            self.status_indicator.set_tooltip_text(None);
            self.status_label.set_text("");
            self.status_label.set_visible(false);
            self.status_picture.set_visible(false);
            self.status_picture.set_alternative_text(None);
            self.status_picture
                .set_paintable(None::<&gtk::gdk::Paintable>);
        }

        self.starred.set_visible(model.starred);
        self.muted.set_visible(model.muted);
        self.external.set_visible(model.external);
        self.huddle.set_visible(model.huddle_active);

        if let Some(label) = model.mention_badge_label() {
            self.mention.set_text(&label);
            self.mention.set_visible(true);
        } else {
            self.mention.set_text("");
            self.mention.set_visible(false);
        }
    }
}

pub(crate) struct SectionHeaderWidgets {
    pub content: gtk::Box,
    pub disclosure: gtk::Image,
    pub header: gtk::Label,
}

impl SectionHeaderWidgets {
    pub fn new() -> Self {
        count_widget_construction();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        content.set_focusable(true);

        count_widget_construction();
        let disclosure = gtk::Image::new();
        disclosure.set_margin_start(9);
        disclosure.set_accessible_role(gtk::AccessibleRole::Presentation);

        count_widget_construction();
        let header = gtk::Label::new(None);
        header.set_xalign(0.0);
        header.set_hexpand(true);
        header.set_margin_top(12);
        header.set_margin_bottom(3);
        header.set_margin_end(9);
        header.add_css_class("caption");
        header.add_css_class("heading");

        content.append(&disclosure);
        content.append(&header);

        Self {
            content,
            disclosure,
            header,
        }
    }
}

pub(crate) struct ThreadSummaryWidgets {
    pub label: gtk::Label,
}

impl ThreadSummaryWidgets {
    pub fn new() -> Self {
        count_widget_construction();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_margin_start(24);
        label.set_margin_end(6);
        label.set_margin_top(2);
        label.set_margin_bottom(2);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.add_css_class("caption");
        label.add_css_class("dim-label");

        Self { label }
    }
}

pub(crate) struct PlaceholderWidgets {
    pub label: gtk::Label,
}

impl PlaceholderWidgets {
    pub fn new() -> Self {
        count_widget_construction();
        let label = gtk::Label::new(None);
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_xalign(0.0);
        label.add_css_class("dim-label");

        Self { label }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarItemVariantKind {
    Conversation,
    SectionHeader,
    ThreadSummary,
    Placeholder,
}

type SidebarContextMenuHandler =
    Rc<dyn Fn(&gtk::Widget, &str, Option<gtk::gdk::Rectangle>) -> Option<gtk::Popover>>;

struct SidebarItemSkeleton {
    conversation: ConversationRowWidgets,
    section_header: SectionHeaderWidgets,
    thread_summary: ThreadSummaryWidgets,
    placeholder: PlaceholderWidgets,
    active_kind: Cell<Option<SidebarItemVariantKind>>,
    last_rendered: RefCell<Option<(KeyedSidebarItem, usize)>>,
    bound_channel_id: RefCell<Option<String>>,
    active_popover: RefCell<Option<gtk::glib::WeakRef<gtk::Popover>>>,
    status_generation: Rc<Cell<u64>>,
}

impl SidebarItemSkeleton {
    fn new(on_context_menu: &SidebarContextMenuHandler) -> Rc<Self> {
        #[cfg(test)]
        SIDEBAR_SKELETON_BUILDS.with(|c| c.set(c.get() + 1));

        let conversation = ConversationRowWidgets::new(SidebarRowLayout::sidebar());
        let section_header = SectionHeaderWidgets::new();
        let thread_summary = ThreadSummaryWidgets::new();
        let placeholder = PlaceholderWidgets::new();

        let skeleton = Rc::new(Self {
            conversation,
            section_header,
            thread_summary,
            placeholder,
            active_kind: Cell::new(None),
            last_rendered: RefCell::new(None),
            bound_channel_id: RefCell::new(None),
            active_popover: RefCell::new(None),
            status_generation: Rc::new(Cell::new(0)),
        });

        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let weak_row = skeleton.conversation.content.downgrade();
        let weak_skel_for_pointer = Rc::downgrade(&skeleton);
        let menu_for_pointer = Rc::clone(on_context_menu);
        gesture.connect_pressed(move |_, _, x, y| {
            let Some(skel) = weak_skel_for_pointer.upgrade() else {
                return;
            };
            let Some(row_for_menu) = weak_row.upgrade() else {
                return;
            };
            skel.open_context_menu(
                &menu_for_pointer,
                &row_for_menu,
                gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1),
            );
        });
        skeleton.conversation.content.add_controller(gesture);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_row = skeleton.conversation.content.downgrade();
        let weak_skel_for_kb = Rc::downgrade(&skeleton);
        let menu_for_kb = Rc::clone(on_context_menu);
        keys.connect_key_pressed(move |_, key, _, state| {
            if !sidebar_context_menu_key(key, state) {
                return gtk::glib::Propagation::Proceed;
            }
            let Some(skel) = weak_skel_for_kb.upgrade() else {
                return gtk::glib::Propagation::Proceed;
            };
            let Some(row_for_menu) = weak_row.upgrade() else {
                return gtk::glib::Propagation::Proceed;
            };
            if skel.open_context_menu(
                &menu_for_kb,
                &row_for_menu,
                gtk::gdk::Rectangle::new(row_for_menu.width() / 2, row_for_menu.height() / 2, 1, 1),
            ) {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        skeleton.conversation.content.add_controller(keys);

        skeleton
    }

    fn open_context_menu(
        &self,
        on_context_menu: &SidebarContextMenuHandler,
        row_for_menu: &gtk::Box,
        pointing_to: gtk::gdk::Rectangle,
    ) -> bool {
        let Some(channel_id) = self.bound_channel_id.borrow().clone() else {
            return false;
        };
        self.popdown_context_menu();
        if let Some(popover) = on_context_menu(
            row_for_menu.upcast_ref::<gtk::Widget>(),
            &channel_id,
            Some(pointing_to),
        ) {
            *self.active_popover.borrow_mut() = Some(popover.downgrade());
        }
        true
    }

    fn popdown_context_menu(&self) {
        let weak_popover = { self.active_popover.borrow_mut().take() };
        if let Some(popover) = weak_popover.and_then(|w| w.upgrade()) {
            popover.popdown();
            if popover.parent().is_some() {
                popover.unparent();
            }
        }
    }

    fn reset_for_pool(&self) {
        *self.bound_channel_id.borrow_mut() = None;
        *self.last_rendered.borrow_mut() = None;
        self.active_kind.set(None);
        let current_gen = self.status_generation.get().saturating_add(1);
        self.status_generation.set(current_gen);
        self.popdown_context_menu();
    }
}

/// Bounded free-list of detached `SidebarItemSkeleton`s so `GListStore::splice`
/// (`SidebarProjectionOperation::Update` / `Splice`), which causes GTK4's
/// `GtkListItemManager` to `teardown` removed `ListItem`s and `setup` added ones,
/// reuses pre-built variant widget trees instead of constructing new GTK widgets.
const SIDEBAR_SKELETON_POOL_LIMIT: usize = 64;

pub(crate) fn build_sidebar_factory(
    custom_emojis: impl Fn() -> Arc<HashMap<String, String>> + 'static,
    on_context_menu: impl Fn(&gtk::Widget, &str, Option<gtk::gdk::Rectangle>) -> Option<gtk::Popover>
        + 'static,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let skeletons: Rc<RefCell<HashMap<usize, Rc<SidebarItemSkeleton>>>> = Rc::default();
    let free_skeletons: Rc<RefCell<Vec<Rc<SidebarItemSkeleton>>>> = Rc::default();
    let custom_emojis: Rc<dyn Fn() -> Arc<HashMap<String, String>>> = Rc::new(custom_emojis);
    let on_context_menu: SidebarContextMenuHandler = Rc::new(on_context_menu);

    {
        let skeletons = Rc::clone(&skeletons);
        let free_skeletons = Rc::clone(&free_skeletons);
        let on_context_menu = Rc::clone(&on_context_menu);
        factory.connect_setup(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let pointer = list_item.as_ptr() as usize;
            let skeleton = free_skeletons
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| SidebarItemSkeleton::new(&on_context_menu));
            list_item.set_child(Some(&skeleton.conversation.content));
            skeleton
                .active_kind
                .set(Some(SidebarItemVariantKind::Conversation));
            skeletons.borrow_mut().insert(pointer, skeleton);
        });
    }

    {
        let skeletons = Rc::clone(&skeletons);
        let custom_emojis = Rc::clone(&custom_emojis);
        factory.connect_bind(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(boxed) = list_item.item().and_downcast::<gtk::glib::BoxedAnyObject>() else {
                return;
            };
            let item = boxed.borrow::<KeyedSidebarItem>().clone();

            let pointer = list_item.as_ptr() as usize;
            let Some(skeleton) = skeletons.borrow().get(&pointer).cloned() else {
                return;
            };

            let (selectable, activatable) = match &item.model {
                SidebarItemModel::Placeholder(_) => (false, false),
                SidebarItemModel::SectionHeader { .. } => (false, true),
                SidebarItemModel::Conversation(_) => (true, true),
                SidebarItemModel::ThreadSummary { .. } => (true, true),
            };
            list_item.set_selectable(selectable);
            list_item.set_activatable(activatable);

            let next_kind = match &item.model {
                SidebarItemModel::Conversation(_) => SidebarItemVariantKind::Conversation,
                SidebarItemModel::SectionHeader { .. } => SidebarItemVariantKind::SectionHeader,
                SidebarItemModel::ThreadSummary { .. } => SidebarItemVariantKind::ThreadSummary,
                SidebarItemModel::Placeholder(_) => SidebarItemVariantKind::Placeholder,
            };

            if let SidebarItemModel::Conversation(model) = &item.model {
                *skeleton.bound_channel_id.borrow_mut() = Some(model.id.clone());
            } else {
                *skeleton.bound_channel_id.borrow_mut() = None;
            }

            let emojis_for_row =
                matches!(next_kind, SidebarItemVariantKind::Conversation).then(|| custom_emojis());
            let emojis_ptr = emojis_for_row
                .as_ref()
                .map_or(0, |emojis| Arc::as_ptr(emojis) as usize);

            if skeleton.active_kind.get() == Some(next_kind) {
                let status_needs_repaint = matches!(
                    &item.model,
                    SidebarItemModel::Conversation(model) if model.status.is_some()
                ) && skeleton.conversation.status_picture.is_visible()
                    && skeleton.conversation.status_picture.paintable().is_none();
                let same = !status_needs_repaint && {
                    let last = skeleton.last_rendered.borrow();
                    if let Some((prev, prev_emojis_ptr)) = &*last {
                        let emojis_unchanged = match &item.model {
                            SidebarItemModel::Conversation(model) if model.status.is_some() => {
                                *prev_emojis_ptr == emojis_ptr
                            }
                            _ => true,
                        };
                        emojis_unchanged
                            && prev.key == item.key
                            && prev.model.renders_same(&item.model)
                    } else {
                        false
                    }
                };
                if same {
                    *skeleton.last_rendered.borrow_mut() = Some((item, emojis_ptr));
                    return;
                }
            }

            if skeleton.active_kind.get() != Some(next_kind) {
                match next_kind {
                    SidebarItemVariantKind::Conversation => {
                        list_item.set_child(Some(&skeleton.conversation.content))
                    }
                    SidebarItemVariantKind::SectionHeader => {
                        list_item.set_child(Some(&skeleton.section_header.content))
                    }
                    SidebarItemVariantKind::ThreadSummary => {
                        list_item.set_child(Some(&skeleton.thread_summary.label))
                    }
                    SidebarItemVariantKind::Placeholder => {
                        list_item.set_child(Some(&skeleton.placeholder.label))
                    }
                }
                skeleton.active_kind.set(Some(next_kind));
            }

            match &item.model {
                SidebarItemModel::Conversation(model) => {
                    let current_gen = skeleton.status_generation.get().saturating_add(1);
                    skeleton.status_generation.set(current_gen);
                    let emojis = emojis_for_row.unwrap_or_default();
                    skeleton.conversation.update(
                        model,
                        &emojis,
                        Rc::clone(&skeleton.status_generation),
                        current_gen,
                    );
                }
                SidebarItemModel::SectionHeader {
                    title, collapsed, ..
                } => {
                    #[cfg(test)]
                    SIDEBAR_ROW_UPDATES.with(|c| c.set(c.get() + 1));
                    let widgets = &skeleton.section_header;
                    widgets.header.set_text(title);
                    widgets.disclosure.set_icon_name(Some(if *collapsed {
                        "pan-end-symbolic"
                    } else {
                        "pan-down-symbolic"
                    }));
                    widgets
                        .content
                        .update_property(&[gtk::accessible::Property::Label(
                            &sidebar_section_accessible_label(title, *collapsed),
                        )]);
                }
                SidebarItemModel::ThreadSummary {
                    reply_count,
                    unread_count,
                    ..
                } => {
                    #[cfg(test)]
                    SIDEBAR_ROW_UPDATES.with(|c| c.set(c.get() + 1));
                    let widgets = &skeleton.thread_summary;
                    widgets
                        .label
                        .set_text(&crate::sidebar::thread_summary_label_text(
                            *reply_count,
                            *unread_count,
                        ));
                }
                SidebarItemModel::Placeholder(placeholder) => {
                    #[cfg(test)]
                    SIDEBAR_ROW_UPDATES.with(|c| c.set(c.get() + 1));
                    let widgets = &skeleton.placeholder;
                    widgets.label.set_text(placeholder.label());
                }
            }

            *skeleton.last_rendered.borrow_mut() = Some((item, emojis_ptr));
        });
    }

    {
        let skeletons = Rc::clone(&skeletons);
        factory.connect_unbind(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let pointer = list_item.as_ptr() as usize;
            let Some(skeleton) = skeletons.borrow().get(&pointer).cloned() else {
                return;
            };

            *skeleton.bound_channel_id.borrow_mut() = None;
            let current_gen = skeleton.status_generation.get().saturating_add(1);
            skeleton.status_generation.set(current_gen);
            skeleton.popdown_context_menu();
        });
    }

    {
        let skeletons = Rc::clone(&skeletons);
        let free_skeletons = Rc::clone(&free_skeletons);
        factory.connect_teardown(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let pointer = list_item.as_ptr() as usize;
            list_item.set_child(None::<&gtk::Widget>);
            let skeleton = skeletons.borrow_mut().remove(&pointer);
            if let Some(skeleton) = skeleton {
                skeleton.reset_for_pool();
                let mut pool = free_skeletons.borrow_mut();
                if pool.len() < SIDEBAR_SKELETON_POOL_LIMIT {
                    pool.push(skeleton);
                }
            }
        });
    }

    factory
}

fn load_sidebar_image_guarded(
    picture: &gtk::Picture,
    url: &str,
    size: i32,
    status_generation: Rc<Cell<u64>>,
    expected_generation: u64,
) {
    let cache_key = (url.to_string(), size);
    let use_cache = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_FILE").is_none();

    if use_cache {
        if let Some(texture) = cached_sidebar_texture(&cache_key) {
            picture.set_paintable(Some(&texture));
            return;
        }
    }
    picture.set_paintable(None::<&gtk::gdk::Paintable>);

    let weak_picture = picture.downgrade();
    if let Some(path) = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::emoji_picker_window::resolve_custom_emoji_cached_file(url))
    {
        decode_sidebar_image_guarded(
            weak_picture,
            &path,
            size,
            use_cache.then_some(cache_key),
            status_generation,
            expected_generation,
        );
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();

    crate::emoji_picker_window::request_custom_emoji_file(url, move |path| {
        let _ = tx.send(path);
    });
    gtk::glib::spawn_future_local(async move {
        if let Some(path) = rx.recv().await {
            decode_sidebar_image_guarded(
                weak_picture,
                &path,
                size,
                use_cache.then_some(cache_key),
                status_generation,
                expected_generation,
            );
        }
    });
}

fn decode_sidebar_image_guarded(
    weak_picture: gtk::glib::WeakRef<gtk::Picture>,
    path: &std::path::Path,
    size: i32,
    cache_key: Option<(String, i32)>,
    status_generation: Rc<Cell<u64>>,
    expected_generation: u64,
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
                    let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                    if let Some(key) = cache_key {
                        store_sidebar_texture(key, &texture);
                    }
                    if status_generation.get() == expected_generation {
                        if let Some(picture) = weak_picture.upgrade() {
                            picture.set_paintable(Some(&texture));
                        }
                    }
                },
            );
        },
    );
}
