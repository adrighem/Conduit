//! Native channel details dialog (AdwDialog) opened from the conversation
//! title: About, Members and Settings tabs.
//!
//! The dialog lives in a thread-local so only one is shown at a time and
//! runtime events can update it in place by channel ID. All state changes go
//! through callbacks into the window, which owns the runtime commands.

mod about;
mod header;
mod members;
mod settings;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;

use crate::channel_details::{members_tab_title, AboutModel, DetailsLayout, MemberRow};
use crate::runtime_mailbox::ConversationTextField;

use about::{build_about, AboutWidgets};
use members::{build_members, MembersWidgets};
use settings::build_settings;

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

    let (header, muted_box, star_button) = header::build_header(input);

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

impl Inner {
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
}
