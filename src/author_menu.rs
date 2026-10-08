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

use crate::message_html::MessageHtmlContext;
use crate::timeline_message_widget::TimelineAction;

/// Menu item: action name, label, enabled flag and action constructor.
type MenuEntry<'a> = (&'a str, String, bool, fn(String) -> TimelineAction);

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
    let entries: [MenuEntry; 2] = [
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

/// Builds "Message…" and "Profile" popover menu anchored to `parent` widget for `user_id`.
pub(crate) fn user_mention_popover(
    parent: &impl IsA<gtk::Widget>,
    user_id: &str,
    context: &MessageHtmlContext,
    on_action: Rc<dyn Fn(TimelineAction)>,
    pointing_to: Option<gtk::gdk::Rectangle>,
) -> Option<gtk::PopoverMenu> {
    let access = author_menu_access(
        Some(user_id),
        context.current_user_id.as_deref(),
        &context.bot_user_ids,
    );
    if !access.any() {
        return None;
    }
    let group = gio::SimpleActionGroup::new();
    let menu = gio::Menu::new();
    let entries: [MenuEntry; 2] = [
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

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(parent);
    popover.set_has_arrow(true);
    if let Some(rect) = pointing_to {
        popover.set_pointing_to(Some(&rect));
    }
    popover.insert_action_group("author", Some(&group));
    let popover_weak = popover.downgrade();
    popover.connect_closed(move |_| {
        if let Some(p) = popover_weak.upgrade() {
            p.unparent();
        }
    });
    Some(popover)
}

/// Shows "Message…" and "Profile" popover menu anchored to `parent` widget for `user_id`.
pub(crate) fn show_user_mention_popover(
    parent: &impl IsA<gtk::Widget>,
    user_id: &str,
    context: &MessageHtmlContext,
    on_action: Rc<dyn Fn(TimelineAction)>,
    pointing_to: Option<gtk::gdk::Rectangle>,
) {
    if let Some(popover) = user_mention_popover(parent, user_id, context, on_action, pointing_to) {
        popover.popup();
    }
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

    #[test]
    fn user_mention_popover_builds_menu_and_popover() {
        crate::timeline_message_widget::tests::run_gtk_test(|| {
            let label = gtk::Label::new(None);
            let context = MessageHtmlContext {
                current_user_id: Some("U_SELF".to_string()),
                ..Default::default()
            };
            let action_taken = Rc::new(std::cell::Cell::new(false));
            let on_action = {
                let action_taken = action_taken.clone();
                Rc::new(move |_action| {
                    action_taken.set(true);
                })
            };
            let popover = user_mention_popover(&label, "U_OTHER", &context, on_action, None);
            assert!(popover.is_some());
            assert!(!action_taken.get());
        });
    }
}
