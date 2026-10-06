//! Native channel details dialog (AdwDialog) opened from the conversation
//! title: About, Members and Settings tabs.
//!
//! The dialog lives in a thread-local so only one is shown at a time and
//! runtime events can update it in place by channel ID. All state changes go
//! through callbacks into the window, which owns the runtime commands.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;

use crate::channel_details::{
    created_by_text, filter_members, members_tab_title, sort_members, AboutModel, DetailsLayout,
    MemberRow,
};
use crate::runtime_mailbox::ConversationTextField;

/// Window-side hooks. All callbacks run on the main thread.
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub(crate) on_star: Rc<dyn Fn(String, bool)>,
    pub(crate) on_edit: Rc<dyn Fn(String, ConversationTextField, String)>,
    pub(crate) on_leave: Rc<dyn Fn(String)>,
    pub(crate) on_profile: Rc<dyn Fn(String)>,
    pub(crate) resolve_member: Rc<dyn Fn(&str) -> MemberRow>,
    pub(crate) user_name: Rc<dyn Fn(&str) -> Option<String>>,
    /// Slack mrkdwn to plain text (unicode emoji, no markup).
    pub(crate) plain_text: Rc<dyn Fn(&str) -> String>,
}

/// Snapshot of what the window knows about the conversation.
#[derive(Clone)]
pub(crate) struct DetailsInput {
    pub(crate) channel_id: String,
    pub(crate) title: String,
    pub(crate) layout: DetailsLayout,
    pub(crate) is_private: bool,
    pub(crate) muted: bool,
    pub(crate) starred: bool,
    pub(crate) about: AboutModel,
    /// Whether a fresh `conversations.info` has been applied.
    pub(crate) loaded: bool,
    pub(crate) utc_offset_secs: i64,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

struct Inner {
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    channel_id: String,
    callbacks: Callbacks,
    star_button: gtk::ToggleButton,
    muted_box: gtk::Box,
    updating_star: Cell<bool>,
    about: Option<AboutWidgets>,
    members_page: RefCell<Option<adw::ViewStackPage>>,
    members: MembersWidgets,
    member_ids: RefCell<Vec<String>>,
    member_total: Cell<Option<usize>>,
    members_complete: Cell<bool>,
}

struct AboutWidgets {
    topic: adw::ActionRow,
    purpose: adw::ActionRow,
    created: adw::ActionRow,
    edit_buttons: Vec<gtk::Button>,
    /// Raw mrkdwn, so editing keeps `:emoji:` shortcodes intact.
    raw_topic: Rc<RefCell<String>>,
    raw_purpose: Rc<RefCell<String>>,
    creator_id: Rc<RefCell<Option<String>>>,
}

struct MembersWidgets {
    rows: Rc<RefCell<HashMap<String, MemberRow>>>,
    visible: Rc<RefCell<Vec<MemberRow>>>,
    store: gtk::StringList,
    search: gtk::SearchEntry,
    pages: gtk::Stack,
    footer: gtk::Label,
    error_page: adw::StatusPage,
}

fn current_for(channel_id: &str) -> Option<Rc<Inner>> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|inner| inner.channel_id == channel_id)
            .cloned()
    })
}

/// Presents the details dialog, replacing any open one.
pub(crate) fn present(parent: &impl IsA<gtk::Widget>, input: &DetailsInput, callbacks: Callbacks) {
    let previous = CURRENT.with(|current| current.borrow_mut().take());
    if let Some(previous) = previous {
        previous.dialog.force_close();
    }
    let inner = build(input, callbacks);
    CURRENT.with(|current| *current.borrow_mut() = Some(inner.clone()));
    let closed = inner.dialog.clone();
    inner.dialog.connect_closed(move |_| {
        CURRENT.with(|current| {
            let is_current = current
                .borrow()
                .as_ref()
                .is_some_and(|inner| inner.dialog == closed);
            if is_current {
                current.borrow_mut().take();
            }
        });
    });
    inner.update_info(input);
    inner.dialog.present(Some(parent));
}

/// Applies fresh conversation data to the open dialog for `input.channel_id`.
pub(crate) fn update(input: &DetailsInput) {
    if let Some(inner) = current_for(&input.channel_id) {
        inner.update_info(input);
    }
}

/// Appends a page of member IDs to the open dialog.
pub(crate) fn append_members(channel_id: &str, user_ids: &[String], complete: bool) {
    if let Some(inner) = current_for(channel_id) {
        {
            let mut ids = inner.member_ids.borrow_mut();
            for id in user_ids {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }
        inner.members_complete.set(complete);
        inner.rebuild_members();
    }
}

/// Closes the dialog for `channel_id`, e.g. after leaving it.
pub(crate) fn close(channel_id: &str) {
    if let Some(inner) = current_for(channel_id) {
        inner.dialog.force_close();
    }
}

/// Reports a failed load or edit: toast, plus an error page when no members
/// were loaded yet.
pub(crate) fn show_error(channel_id: &str, message: &str) {
    if let Some(inner) = current_for(channel_id) {
        inner.toasts.add_toast(adw::Toast::new(message));
        if let Some(about) = &inner.about {
            about
                .edit_buttons
                .iter()
                .for_each(|b| b.set_sensitive(true));
        }
        if inner.member_ids.borrow().is_empty() && !inner.members_complete.get() {
            inner.members.error_page.set_description(Some(message));
            inner.members.pages.set_visible_child_name("error");
        }
    }
}

fn build(input: &DetailsInput, callbacks: Callbacks) -> Rc<Inner> {
    let stack = adw::ViewStack::new();
    stack.set_vexpand(true);

    let about = input.layout.about.then(|| {
        let (page, widgets) = build_about(input, &callbacks);
        let _ = stack.add_titled(&page, Some("about"), &gettext("About"));
        widgets
    });
    let (members_widget, members) = build_members(&callbacks);
    let members_page = stack.add_titled(&members_widget, Some("members"), &members_tab_title(None));
    if input.layout.settings {
        let page = build_settings(input, &callbacks);
        let _ = stack.add_titled(&page, Some("settings"), &gettext("Settings"));
    }

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    header.set_margin_start(18);
    header.set_margin_end(18);
    if input.is_private {
        let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
        lock.set_tooltip_text(Some(&gettext("Private")));
        header.append(&lock);
    }
    let title = gtk::Label::new(Some(&input.title));
    title.add_css_class("title-2");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_selectable(true);
    header.append(&title);

    let muted_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    muted_box.add_css_class("dim-label");
    muted_box.append(&gtk::Image::from_icon_name(
        "notifications-disabled-symbolic",
    ));
    muted_box.append(&gtk::Label::new(Some(&gettext("Muted"))));
    header.append(&muted_box);

    let star_button = gtk::ToggleButton::new();
    star_button.add_css_class("flat");
    star_button.set_valign(gtk::Align::Center);
    header.append(&star_button);

    let toolbar = adw::ToolbarView::new();
    let header_bar = adw::HeaderBar::new();
    let switcher = adw::InlineViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_homogeneous(true);
    switcher.set_margin_start(18);
    switcher.set_margin_end(18);
    switcher.set_margin_bottom(6);
    if stack.pages().n_items() <= 1 {
        header_bar.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Members"), "")));
    }
    toolbar.add_top_bar(&header_bar);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    if stack.pages().n_items() > 1 {
        content.append(&switcher);
    }
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&stack);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));
    toolbar.set_content(Some(&toasts));

    let dialog = adw::Dialog::new();
    dialog.set_title(&gettext("Conversation details"));
    dialog.set_content_width(520);
    dialog.set_content_height(620);
    dialog.set_child(Some(&toolbar));

    let inner = Rc::new(Inner {
        dialog,
        toasts,
        channel_id: input.channel_id.clone(),
        callbacks,
        star_button,
        muted_box,
        updating_star: Cell::new(false),
        about,
        members_page: RefCell::new(Some(members_page)),
        members,
        member_ids: RefCell::new(Vec::new()),
        member_total: Cell::new(input.about.member_count),
        members_complete: Cell::new(false),
    });
    inner.connect_star();
    inner.connect_member_search();
    inner
}

fn expand_action_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle_selectable(true);
    row.add_css_class("property");
    row
}

fn build_about(
    input: &DetailsInput,
    callbacks: &Callbacks,
) -> (adw::PreferencesPage, AboutWidgets) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    let topic = expand_action_row(&gettext("Topic"));
    let purpose = expand_action_row(&gettext("Description"));
    let created = expand_action_row(&gettext("Created by"));
    let mut edit_buttons = Vec::new();
    let raw_topic: Rc<RefCell<String>> = Rc::default();
    let raw_purpose: Rc<RefCell<String>> = Rc::default();
    let creator_id: Rc<RefCell<Option<String>>> = Rc::default();
    let editable = [
        (
            &topic,
            ConversationTextField::Topic,
            gettext("Edit topic"),
            &raw_topic,
        ),
        (
            &purpose,
            ConversationTextField::Purpose,
            gettext("Edit description"),
            &raw_purpose,
        ),
    ];
    for (row, field, label, raw) in editable {
        let button = gtk::Button::from_icon_name("document-edit-symbolic");
        button.add_css_class("flat");
        button.set_valign(gtk::Align::Center);
        button.set_tooltip_text(Some(&label));
        button.update_property(&[gtk::accessible::Property::Label(&label)]);
        row.add_suffix(&button);
        edit_buttons.push(button.clone());
        let channel_id = input.channel_id.clone();
        let callbacks = callbacks.clone();
        let row_title = row.title().to_string();
        let raw = raw.clone();
        button.connect_clicked(move |button| {
            let current = raw.borrow().clone();
            edit_text_dialog(button, &row_title, &current, &callbacks, &channel_id, field);
        });
    }
    group.add(&topic);
    group.add(&purpose);
    group.add(&created);
    page.add(&group);
    {
        let creator_id = creator_id.clone();
        let profile = callbacks.on_profile.clone();
        created.connect_activated(move |_| {
            if let Some(id) = creator_id.borrow().clone() {
                profile(id);
            }
        });
    }

    let footer = adw::PreferencesGroup::new();
    let id_row = expand_action_row(&gettext("Channel ID"));
    id_row.set_subtitle(&input.channel_id);
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.add_css_class("flat");
    copy.set_valign(gtk::Align::Center);
    let tooltip = gettext("Copy channel ID");
    copy.set_tooltip_text(Some(&tooltip));
    copy.update_property(&[gtk::accessible::Property::Label(&tooltip)]);
    id_row.add_suffix(&copy);
    footer.add(&id_row);
    page.add(&footer);
    let channel_id = input.channel_id.clone();
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&channel_id);
        if let Some(inner) = current_for(&channel_id) {
            inner
                .toasts
                .add_toast(adw::Toast::new(&gettext("Channel ID copied")));
        }
    });
    (
        page,
        AboutWidgets {
            topic,
            purpose,
            created,
            edit_buttons,
            raw_topic,
            raw_purpose,
            creator_id,
        },
    )
}

fn edit_text_dialog(
    anchor: &gtk::Button,
    label: &str,
    initial: &str,
    callbacks: &Callbacks,
    channel_id: &str,
    field: ConversationTextField,
) {
    let heading = gettext("Edit {field}").replace("{field}", &label.to_lowercase());
    let alert = adw::AlertDialog::new(Some(&heading), None);
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_max_length(250);
    entry.set_activates_default(true);
    entry.update_property(&[gtk::accessible::Property::Label(label)]);
    alert.set_extra_child(Some(&entry));
    alert.add_response("cancel", &gettext("Cancel"));
    alert.add_response("save", &gettext("Save"));
    alert.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    alert.set_default_response(Some("save"));
    alert.set_close_response("cancel");
    let callbacks = callbacks.clone();
    let channel_id = channel_id.to_string();
    let initial = initial.to_string();
    alert.connect_response(Some("save"), move |_, _| {
        let text = entry.text().trim().to_string();
        if text == initial.trim() {
            return;
        }
        if let Some(inner) = current_for(&channel_id) {
            if let Some(about) = &inner.about {
                about
                    .edit_buttons
                    .iter()
                    .for_each(|b| b.set_sensitive(false));
            }
        }
        (callbacks.on_edit)(channel_id.clone(), field, text);
    });
    alert.present(Some(anchor));
}

fn build_settings(input: &DetailsInput, callbacks: &Callbacks) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_description(Some(&if input.is_private {
        gettext("You will need an invitation to rejoin this private channel.")
    } else {
        gettext("You can rejoin this channel later.")
    }));
    let leave = adw::ButtonRow::new();
    leave.set_title(&gettext("Leave channel"));
    leave.add_css_class("destructive-action");
    group.add(&leave);
    page.add(&group);

    let channel_id = input.channel_id.clone();
    let title = input.title.clone();
    let is_private = input.is_private;
    let callbacks = callbacks.clone();
    leave.connect_activated(move |button| {
        let heading = gettext("Leave {name}?").replace("{name}", &title);
        let body = if is_private {
            gettext("You won't be able to rejoin this private channel unless someone invites you again.")
        } else {
            gettext("You will stop receiving messages from this channel.")
        };
        let alert = adw::AlertDialog::new(Some(&heading), Some(&body));
        alert.add_response("cancel", &gettext("Cancel"));
        alert.add_response("leave", &gettext("Leave channel"));
        alert.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        let channel_id = channel_id.clone();
        let callbacks = callbacks.clone();
        alert.connect_response(Some("leave"), move |_, _| {
            (callbacks.on_leave)(channel_id.clone());
            close(&channel_id);
        });
        alert.present(Some(button));
    });
    page.upcast()
}

fn build_members(callbacks: &Callbacks) -> (gtk::Box, MembersWidgets) {
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
    fn connect_star(self: &Rc<Self>) {
        let channel_id = self.channel_id.clone();
        let on_star = self.callbacks.on_star.clone();
        let weak = Rc::downgrade(self);
        self.star_button.connect_toggled(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.updating_star.get() {
                return;
            }
            let active = inner.star_button.is_active();
            inner.sync_star_appearance(active);
            on_star(channel_id.clone(), active);
        });
    }

    fn connect_member_search(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.members.search.connect_search_changed(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.apply_member_filter();
            }
        });
    }

    fn sync_star_appearance(&self, starred: bool) {
        self.star_button.set_icon_name(if starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        let label = if starred {
            gettext("Remove star")
        } else {
            gettext("Star conversation")
        };
        self.star_button.set_tooltip_text(Some(&label));
        self.star_button
            .update_property(&[gtk::accessible::Property::Label(&label)]);
    }

    fn update_info(&self, input: &DetailsInput) {
        self.updating_star.set(true);
        self.star_button.set_active(input.starred);
        self.sync_star_appearance(input.starred);
        self.updating_star.set(false);
        self.muted_box.set_visible(input.muted);
        if input.about.member_count.is_some() {
            self.member_total.set(input.about.member_count);
        }
        self.refresh_members_title();
        if let Some(about) = &self.about {
            self.update_about(about, input);
        }
    }

    fn update_about(&self, about: &AboutWidgets, input: &DetailsInput) {
        let plain = &self.callbacks.plain_text;
        for (row, raw, store, empty) in [
            (
                &about.topic,
                &input.about.topic,
                &about.raw_topic,
                gettext("Add a topic"),
            ),
            (
                &about.purpose,
                &input.about.purpose,
                &about.raw_purpose,
                gettext("Add a description"),
            ),
        ] {
            *store.borrow_mut() = raw.clone();
            let text = plain(raw);
            row.set_subtitle(if text.is_empty() { &empty } else { &text });
            if text.is_empty() {
                row.add_css_class("dim-label");
            } else {
                row.remove_css_class("dim-label");
            }
        }
        about
            .edit_buttons
            .iter()
            .for_each(|b| b.set_sensitive(true));

        let creator_name = input
            .about
            .creator_id
            .as_deref()
            .map(|id| (self.callbacks.user_name)(id).unwrap_or_else(|| id.to_string()));
        let text = created_by_text(
            creator_name.as_deref(),
            input.about.created,
            input.utc_offset_secs,
        );
        match text {
            Some(text) => {
                about.created.set_subtitle(&text);
                about.created.set_visible(true);
            }
            None if input.loaded => about.created.set_visible(false),
            None => about.created.set_subtitle(&gettext("Loading...")),
        }
        *about.creator_id.borrow_mut() = input.about.creator_id.clone();
        about
            .created
            .set_activatable(input.about.creator_id.is_some());
    }

    fn refresh_members_title(&self) {
        let count = if self.members_complete.get() {
            Some(self.member_ids.borrow().len())
        } else {
            self.member_total.get()
        };
        if let Some(page) = self.members_page.borrow().as_ref() {
            page.set_title(Some(&members_tab_title(count)));
        }
    }

    fn rebuild_members(&self) {
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

    fn apply_member_filter(&self) {
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
