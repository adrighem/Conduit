//! Quick-action menu on a timeline message's author (avatar and name).
//!
//! The access rules are pure so they can be tested without GTK; the widget
//! builder wraps the already-built avatar and name in a flat `MenuButton`,
//! which gives pointer cursor, keyboard activation and popover anchoring.

use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::gio;

use crate::timeline_message_widget::TimelineAction;

/// Which author menu entries are usable for a given author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthorMenuAccess {
    pub(crate) message: bool,
    pub(crate) profile: bool,
}

impl AuthorMenuAccess {
    pub(crate) const NONE: Self = Self {
        message: false,
        profile: false,
    };

    pub(crate) fn any(self) -> bool {
        self.message || self.profile
    }
}

/// Decides the menu entries for the person heading a message.
///
/// Bots and app identities (no human user id) get no menu at all. The signed
/// in user can open their own profile but not start a DM with themselves.
pub(crate) fn author_menu_access(
    user_id: Option<&str>,
    current_user_id: Option<&str>,
    bot_user_ids: &HashSet<String>,
) -> AuthorMenuAccess {
    let Some(user_id) = user_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return AuthorMenuAccess::NONE;
    };
    if bot_user_ids.contains(user_id) {
        return AuthorMenuAccess::NONE;
    }
    AuthorMenuAccess {
        message: current_user_id != Some(user_id),
        profile: true,
    }
}

fn author_menu_label(display_name: &str) -> String {
    gettext("Open menu for {name}").replace("{name}", display_name)
}

/// Wraps `avatar` and `name` in a flat menu button offering "Message…" and
/// "Profile". `on_action` is invoked with the chosen [`TimelineAction`].
pub(crate) fn author_menu_button(
    avatar: &gtk::Widget,
    name: &gtk::Label,
    user_id: &str,
    display_name: &str,
    access: AuthorMenuAccess,
    on_action: Rc<dyn Fn(TimelineAction)>,
) -> gtk::MenuButton {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.append(avatar);
    content.append(name);

    let group = gio::SimpleActionGroup::new();
    let menu = gio::Menu::new();
    let entries: [(&str, String, bool, fn(String) -> TimelineAction); 2] = [
        (
            "message",
            gettext("Message…"),
            access.message,
            TimelineAction::MessageUser,
        ),
        (
            "profile",
            gettext("Profile"),
            access.profile,
            TimelineAction::ShowProfile,
        ),
    ];
    for (name, label, enabled, make_action) in entries {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(enabled);
        let on_action = on_action.clone();
        let user_id = user_id.to_string();
        action.connect_activate(move |_, _| on_action(make_action(user_id.clone())));
        group.add_action(&action);
        menu.append(Some(&label), Some(&format!("author.{name}")));
    }

    let button = gtk::MenuButton::new();
    button.set_child(Some(&content));
    button.set_menu_model(Some(&menu));
    button.set_has_frame(false);
    button.set_focus_on_click(false);
    button.set_halign(gtk::Align::Start);
    button.set_cursor_from_name(Some("pointer"));
    button.add_css_class("author-menu-button");
    button.insert_action_group("author", Some(&group));
    let label = author_menu_label(display_name);
    button.set_tooltip_text(Some(&label));
    button.update_property(&[gtk::accessible::Property::Label(&label)]);
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bots() -> HashSet<String> {
        HashSet::from(["UBOT".to_string()])
    }

    #[test]
    fn normal_user_gets_message_and_profile() {
        let access = author_menu_access(Some("U1"), Some("U2"), &bots());
        assert_eq!(
            access,
            AuthorMenuAccess {
                message: true,
                profile: true
            }
        );
    }

    #[test]
    fn self_cannot_message_but_can_view_profile() {
        let access = author_menu_access(Some("U2"), Some("U2"), &bots());
        assert_eq!(
            access,
            AuthorMenuAccess {
                message: false,
                profile: true
            }
        );
    }

    #[test]
    fn bots_and_identity_less_authors_get_no_menu() {
        assert_eq!(
            author_menu_access(Some("UBOT"), Some("U2"), &bots()),
            AuthorMenuAccess::NONE
        );
        assert_eq!(
            author_menu_access(None, Some("U2"), &bots()),
            AuthorMenuAccess::NONE
        );
        assert_eq!(
            author_menu_access(Some("  "), None, &bots()),
            AuthorMenuAccess::NONE
        );
        assert!(!AuthorMenuAccess::NONE.any());
    }

    #[test]
    fn unknown_self_still_allows_message() {
        let access = author_menu_access(Some("U1"), None, &bots());
        assert!(access.message && access.profile);
    }
}
