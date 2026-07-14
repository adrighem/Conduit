/* application.rs
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

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::glib::variant::{StaticVariantType, ToVariant};
use gtk::{gio, glib};
use sha2::{Digest, Sha256};

use crate::config::{self, VERSION};
use crate::shortcuts::APP_SHORTCUTS;
use crate::ConduitWindow;

const OPEN_CONVERSATION_ACTION: &str = "app.open-conversation";
const ABOUT_ICON_NAME: &str = config::APPLICATION_ID;
const ABOUT_LOGO_SIZE: i32 = 192;

fn resize_about_logo(dialog: &adw::AboutDialog) -> bool {
    let mut widgets = vec![dialog.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = widgets.pop() {
        if let Ok(image) = widget.clone().downcast::<gtk::Image>() {
            if image.icon_name().as_deref() == Some(ABOUT_ICON_NAME) {
                image.set_pixel_size(ABOUT_LOGO_SIZE);
                return true;
            }
        }

        let mut child = widget.first_child();
        while let Some(current) = child {
            child = current.next_sibling();
            widgets.push(current);
        }
    }
    false
}

fn conversation_notification_id(workspace_id: &str, channel_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(workspace_id.as_bytes());
    digest.update([0]);
    digest.update(channel_id.as_bytes());
    format!("message:{:x}", digest.finalize())
}

fn conversation_target_variant(workspace_id: &str, channel_id: &str) -> glib::Variant {
    (workspace_id, channel_id).to_variant()
}

fn conversation_target_from_variant(target: &glib::Variant) -> Option<(String, String)> {
    let (workspace_id, channel_id) = target.get::<(String, String)>()?;
    let workspace_id = workspace_id.trim();
    let channel_id = channel_id.trim();
    (!workspace_id.is_empty() && !channel_id.is_empty())
        .then(|| (workspace_id.to_string(), channel_id.to_string()))
}

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Debug, Default)]
    pub struct ConduitApplication {
        search_provider_registration: RefCell<Option<gio::RegistrationId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConduitApplication {
        const NAME: &'static str = "ConduitApplication";
        type Type = super::ConduitApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for ConduitApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_options();
            obj.setup_gactions();
        }
    }

    impl ApplicationImpl for ConduitApplication {
        fn dbus_register(
            &self,
            connection: &gio::DBusConnection,
            object_path: &str,
        ) -> Result<(), glib::Error> {
            self.parent_dbus_register(connection, object_path)?;
            let registration =
                crate::gnome_search_provider::register(connection, self.obj().as_ref())?;
            self.search_provider_registration
                .borrow_mut()
                .replace(registration);
            Ok(())
        }

        fn dbus_unregister(&self, connection: &gio::DBusConnection, object_path: &str) {
            if let Some(registration) = self.search_provider_registration.borrow_mut().take() {
                let _ = connection.unregister_object(registration);
            }
            self.parent_dbus_unregister(connection, object_path);
        }

        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let application = self.obj();
            crate::debug::set_enabled(false);
            application.present_window(false, false);
        }

        fn command_line(&self, command_line: &gio::ApplicationCommandLine) -> glib::ExitCode {
            let application = self.obj();
            let options = command_line.options_dict();
            let connect = options.contains("connect");
            let debug = options.contains("debug");
            let debug_auth = debug || options.contains("debug-auth");
            crate::debug::set_enabled(debug);
            crate::debug::log("app", "debug logging enabled");
            application.present_window(connect, debug_auth);
            0.into()
        }

        fn shutdown(&self) {
            self.obj().flush_active_window_drafts();
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for ConduitApplication {}
    impl AdwApplicationImpl for ConduitApplication {}
}

glib::wrapper! {
    pub struct ConduitApplication(ObjectSubclass<imp::ConduitApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl ConduitApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/eu/vanadrighem/conduit")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let shortcuts_action = gio::ActionEntry::builder("shortcuts")
            .activate(move |app: &Self, _, _| app.show_shortcuts())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let open_conversation_action = gio::ActionEntry::builder("open-conversation")
            .parameter_type(Some(<(String, String)>::static_variant_type().as_ref()))
            .activate(move |app: &Self, _, target| {
                let Some((workspace_id, channel_id)) =
                    target.and_then(conversation_target_from_variant)
                else {
                    return;
                };
                app.present_conversation_target(workspace_id, channel_id);
            })
            .build();
        self.add_action_entries([
            quit_action,
            shortcuts_action,
            preferences_action,
            about_action,
            open_conversation_action,
        ]);
        for shortcut in APP_SHORTCUTS {
            self.set_accels_for_action(shortcut.action, shortcut.accelerators);
        }
    }

    fn setup_options(&self) {
        self.add_main_option(
            "connect",
            glib::Char::from(b'c'),
            glib::OptionFlags::NONE,
            glib::OptionArg::None,
            "Open the Slack workspace connection flow",
            None,
        );
        self.add_main_option(
            "debug",
            glib::Char::from(b'd'),
            glib::OptionFlags::NONE,
            glib::OptionArg::None,
            "Print renderer and Slack loading diagnostics to stderr",
            None,
        );
        self.add_main_option(
            "debug-auth",
            glib::Char::from(b'\0'),
            glib::OptionFlags::NONE,
            glib::OptionArg::None,
            "Print Slack OAuth diagnostics to stderr",
            None,
        );
    }

    fn present_window(&self, connect: bool, debug_auth: bool) {
        self.configure_icon_theme();

        let window = self.active_window().unwrap_or_else(|| {
            let window = ConduitWindow::new(self);
            window.upcast()
        });

        if let Ok(conduit_window) = window.clone().downcast::<ConduitWindow>() {
            conduit_window.set_auth_debug(debug_auth);
            if connect {
                conduit_window.show_connect_requested();
            }
        }

        window.present();
    }

    fn flush_active_window_drafts(&self) {
        if let Some(window) = self
            .active_window()
            .and_then(|window| window.downcast::<ConduitWindow>().ok())
        {
            window.flush_drafts();
        }
    }

    fn present_conversation_target(&self, workspace_id: String, channel_id: String) {
        self.present_window(false, false);
        let Some(window) = self
            .active_window()
            .and_then(|window| window.downcast::<ConduitWindow>().ok())
        else {
            return;
        };
        window.open_notification_target(workspace_id, channel_id);
    }

    pub(crate) fn send_conversation_notification(
        &self,
        workspace_id: &str,
        channel_id: &str,
        title: &str,
        body: &str,
    ) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        let target = conversation_target_variant(workspace_id, channel_id);
        notification.set_default_action_and_target_value(OPEN_CONVERSATION_ACTION, Some(&target));
        self.send_notification(
            Some(&conversation_notification_id(workspace_id, channel_id)),
            &notification,
        );
    }

    pub(crate) fn withdraw_conversation_notification(&self, workspace_id: &str, channel_id: &str) {
        self.withdraw_notification(&conversation_notification_id(workspace_id, channel_id));
    }

    fn configure_icon_theme(&self) {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };

        gtk::IconTheme::for_display(&display).add_resource_path("/eu/vanadrighem/conduit/icons");
    }

    fn show_shortcuts(&self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let builder = gtk::Builder::from_resource("/eu/vanadrighem/conduit/shortcuts-dialog.ui");
        if let Some(dialog) = builder.object::<gtk::ShortcutsWindow>("shortcuts_dialog") {
            dialog.set_transient_for(Some(&window));
            dialog.present();
        }
    }

    fn show_preferences(&self) {
        let Some(window) = self.active_window() else {
            return;
        };

        let settings = gio::Settings::new(config::APPLICATION_ID);
        let unreads_row = adw::SwitchRow::builder()
            .title("Show Unreads section")
            .subtitle("Duplicate unread conversations into a separate sidebar section.")
            .active(settings.boolean(config::SIDEBAR_SHOW_UNREADS_SECTION_KEY))
            .build();
        settings
            .bind(
                config::SIDEBAR_SHOW_UNREADS_SECTION_KEY,
                &unreads_row,
                "active",
            )
            .build();

        let sidebar_group = adw::PreferencesGroup::builder().title("Sidebar").build();
        sidebar_group.add(&unreads_row);

        let page = adw::PreferencesPage::builder()
            .title("Preferences")
            .icon_name("view-list-symbolic")
            .build();
        page.add(&sidebar_group);

        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        dialog.add(&page);
        dialog.present(Some(&window));
    }

    fn show_about(&self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let about = adw::AboutDialog::builder()
            .application_name("Conduit")
            .application_icon(ABOUT_ICON_NAME)
            .developer_name("Vincent van Adrighem")
            .version(VERSION)
            .developers(vec!["Vincent van Adrighem"])
            // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
            .translator_credits(gettext("translator-credits"))
            .copyright("© 2026 Vincent van Adrighem")
            .build();
        // The dialog's internal widget tree is populated when it is mapped.
        // Resizing the logo is cosmetic and must remain non-fatal if libadwaita
        // changes that private widget tree in a future release.
        about.connect_map(|dialog| {
            let _ = resize_about_logo(dialog);
        });

        about.present(Some(&window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_ids_are_stable_scoped_and_opaque() {
        let id = conversation_notification_id("T123", "C123");

        assert_eq!(id, conversation_notification_id("T123", "C123"));
        assert_ne!(id, conversation_notification_id("T999", "C123"));
        assert_ne!(id, conversation_notification_id("T123", "C999"));
        assert!(id.starts_with("message:"));
        assert!(!id.contains("T123"));
        assert!(!id.contains("C123"));
    }

    #[test]
    fn about_logo_is_larger_than_libadwaita_default() {
        const LIBADWAITA_ABOUT_ICON_SIZE: i32 = 128;

        assert_eq!(ABOUT_ICON_NAME, config::APPLICATION_ID);
        const { assert!(ABOUT_LOGO_SIZE > LIBADWAITA_ABOUT_ICON_SIZE) };
    }

    #[test]
    fn notification_action_targets_round_trip_and_reject_empty_parts() {
        let target = conversation_target_variant("T123", "C123");
        assert_eq!(
            conversation_target_from_variant(&target),
            Some(("T123".into(), "C123".into()))
        );

        assert_eq!(
            conversation_target_from_variant(&conversation_target_variant("", "C123")),
            None
        );
        assert_eq!(
            conversation_target_from_variant(&conversation_target_variant("T123", "  ")),
            None
        );
    }
}
