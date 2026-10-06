//! Native user profile dialog (AdwDialog) opened from the timeline author menu.
//!
//! The view model and its formatting are pure functions; the dialog widget
//! lives in a thread-local so that at most one profile is shown at a time and
//! runtime events can update it in place by user id.

mod model;
mod page;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::glib;

pub(crate) use model::ProfileInput;

#[derive(Clone)]
struct ProfileDialog {
    dialog: adw::Dialog,
    user_id: String,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    error_page: adw::StatusPage,
    on_message: Rc<dyn Fn(String)>,
}

thread_local! {
    static CURRENT: RefCell<Option<ProfileDialog>> = const { RefCell::new(None) };
}

fn current_for(user_id: &str) -> Option<ProfileDialog> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|dialog| dialog.user_id == user_id)
            .cloned()
    })
}

/// Presents the profile dialog for `user_id`, replacing any open one. Shows
/// `initial` right away when the user is already known, otherwise a spinner
/// until [`update`] delivers the profile.
pub(crate) fn present(
    parent: &impl IsA<gtk::Widget>,
    user_id: &str,
    initial: Option<ProfileInput>,
    on_message: Rc<dyn Fn(String)>,
) {
    let previous = CURRENT.with(|current| current.borrow_mut().take());
    if let Some(previous) = previous {
        previous.dialog.force_close();
    }

    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.update_property(&[gtk::accessible::Property::Label(&gettext(
        "Loading profile",
    ))]);
    let error_page = adw::StatusPage::new();
    error_page.set_icon_name(Some("dialog-error-symbolic"));
    error_page.set_title(&gettext("Could not load profile"));
    let stack = gtk::Stack::new();
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&error_page, Some("error"));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&toasts));
    let dialog = adw::Dialog::new();
    dialog.set_title(&gettext("Profile"));
    dialog.set_content_width(420);
    dialog.set_content_height(560);
    dialog.set_child(Some(&toolbar));

    let this = ProfileDialog {
        dialog: dialog.clone(),
        user_id: user_id.to_string(),
        stack,
        toasts,
        error_page,
        on_message,
    };
    CURRENT.with(|current| *current.borrow_mut() = Some(this.clone()));
    dialog.connect_closed(glib::clone!(
        #[weak(rename_to = closed)]
        dialog,
        move |_| {
            CURRENT.with(|current| {
                let is_current = current
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.dialog == closed);
                if is_current {
                    current.borrow_mut().take();
                }
            });
        }
    ));
    if let Some(input) = initial {
        this.show_profile(&input);
    }
    dialog.present(Some(parent));
}

/// Replaces the content of the open dialog for `user_id` with `input`.
pub(crate) fn update(user_id: &str, input: &ProfileInput) {
    if let Some(dialog) = current_for(user_id) {
        dialog.show_profile(input);
    }
}

/// Shows an error page if the dialog for `user_id` has nothing to show yet.
pub(crate) fn show_error(user_id: &str, message: &str) {
    if let Some(dialog) = current_for(user_id) {
        if dialog.stack.visible_child_name().as_deref() == Some("loading") {
            dialog.error_page.set_description(Some(message));
            dialog.stack.set_visible_child_name("error");
        }
    }
}
