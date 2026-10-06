//! Members tab: searchable list, loading, empty and error states.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;

use super::{Callbacks, Inner};
use crate::channel_details::{filter_members, members_tab_title, sort_members, MemberRow};

pub(super) struct MembersWidgets {
    pub(super) rows: Rc<RefCell<HashMap<String, MemberRow>>>,
    pub(super) visible: Rc<RefCell<Vec<MemberRow>>>,
    pub(super) store: gtk::StringList,
    pub(super) search: gtk::SearchEntry,
    pub(super) pages: gtk::Stack,
    pub(super) footer: gtk::Label,
    pub(super) error_page: adw::StatusPage,
}

pub(super) fn build_members(callbacks: &Callbacks) -> (gtk::Box, MembersWidgets) {
    let rows: Rc<RefCell<HashMap<String, MemberRow>>> = Rc::default();
    let visible: Rc<RefCell<Vec<MemberRow>>> = Rc::default();
    let store = gtk::StringList::new(&[]);
    let selection = gtk::NoSelection::new(Some(store.clone()));

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.append(&adw::Avatar::new(36, None, true));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_valign(gtk::Align::Center);
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&name);
        text.append(&status);
        row.append(&text);
        item.set_child(Some(&row));
    });
    let bind_rows = rows.clone();
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(id) = item.item().and_downcast::<gtk::StringObject>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let map = bind_rows.borrow();
        let Some(member) = map.get(id.string().as_str()) else {
            return;
        };
        if let Some(avatar) = row.first_child().and_downcast::<adw::Avatar>() {
            avatar.set_text(Some(&member.name));
            let texture = member
                .avatar_path
                .as_deref()
                .and_then(crate::timeline_message_widget::get_or_load_texture);
            avatar.set_custom_image(texture.as_ref());
        }
        if let Some(text) = row.last_child().and_downcast::<gtk::Box>() {
            if let Some(name) = text.first_child().and_downcast::<gtk::Label>() {
                name.set_text(&member.name);
            }
            if let Some(status) = text.last_child().and_downcast::<gtk::Label>() {
                status.set_text(&member.status);
                status.set_visible(!member.status.is_empty());
            }
        }
    });

    let list = gtk::ListView::new(Some(selection), Some(factory));
    list.set_single_click_activate(true);
    list.add_css_class("navigation-sidebar");
    list.update_property(&[gtk::accessible::Property::Label(&gettext("Members"))]);
    let profile = callbacks.on_profile.clone();
    list.connect_activate(move |view, position| {
        let id = view
            .model()
            .and_then(|model| model.item(position))
            .and_downcast::<gtk::StringObject>();
        if let Some(id) = id {
            profile(id.string().to_string());
        }
    });
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_child(Some(&list));

    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.update_property(&[gtk::accessible::Property::Label(&gettext(
        "Loading members",
    ))]);
    let empty = adw::StatusPage::new();
    empty.set_icon_name(Some("system-search-symbolic"));
    empty.set_title(&gettext("No matching members"));
    let error_page = adw::StatusPage::new();
    error_page.set_icon_name(Some("dialog-error-symbolic"));
    error_page.set_title(&gettext("Could not load members"));
    let pages = gtk::Stack::new();
    pages.add_named(&spinner, Some("loading"));
    pages.add_named(&scrolled, Some("list"));
    pages.add_named(&empty, Some("empty"));
    pages.add_named(&error_page, Some("error"));
    pages.set_vexpand(true);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some(&gettext("Search members")));
    search.set_margin_top(12);
    search.set_margin_bottom(6);
    search.set_margin_start(12);
    search.set_margin_end(12);
    let footer = gtk::Label::new(Some(&gettext("Loading more members...")));
    footer.add_css_class("dim-label");
    footer.add_css_class("caption");
    footer.set_margin_top(4);
    footer.set_margin_bottom(6);
    footer.set_visible(false);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&search);
    root.append(&pages);
    root.append(&footer);
    let members = MembersWidgets {
        rows,
        visible,
        store,
        search,
        pages,
        footer,
        error_page,
    };
    (root, members)
}

impl Inner {
    pub(super) fn connect_member_search(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.members.search.connect_search_changed(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.apply_member_filter();
            }
        });
    }

    pub(super) fn refresh_members_title(&self) {
        let count = if self.members_complete.get() {
            Some(self.member_ids.borrow().len())
        } else {
            self.member_total.get()
        };
        if let Some(page) = self.members_page.borrow().as_ref() {
            page.set_title(Some(&members_tab_title(count)));
        }
    }

    pub(super) fn rebuild_members(&self) {
        let resolve = &self.callbacks.resolve_member;
        let mut rows: Vec<MemberRow> = self
            .member_ids
            .borrow()
            .iter()
            .map(|id| resolve(id))
            .collect();
        sort_members(&mut rows);
        *self.members.rows.borrow_mut() = rows
            .iter()
            .map(|row| (row.user_id.clone(), row.clone()))
            .collect();
        *self.members.visible.borrow_mut() = rows;
        self.members
            .footer
            .set_visible(!self.members_complete.get());
        self.refresh_members_title();
        self.apply_member_filter();
    }

    pub(super) fn apply_member_filter(&self) {
        let query = self.members.search.text();
        let ids: Vec<String> = {
            let all = self.members.visible.borrow();
            filter_members(&all, query.as_str())
                .into_iter()
                .map(|row| row.user_id.clone())
                .collect()
        };
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.members
            .store
            .splice(0, self.members.store.n_items(), &refs);
        let page = if !ids.is_empty() {
            "list"
        } else if self.member_ids.borrow().is_empty() && !self.members_complete.get() {
            "loading"
        } else {
            "empty"
        };
        self.members.pages.set_visible_child_name(page);
    }
}
