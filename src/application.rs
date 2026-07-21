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
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;

use crate::auth::AppTokenStore;
use crate::config::{self, VERSION};
use crate::realtime::{RealtimePhase, RealtimeStatus, RealtimeTransport};
use crate::shortcuts::APP_SHORTCUTS;
use crate::slack_link::{parse_slack_uri, SlackUri};
use crate::ConduitWindow;

const OPEN_CONVERSATION_ACTION: &str = "app.open-conversation";
const OPEN_THREAD_ACTION: &str = "app.open-thread";
const ABOUT_ICON_NAME: &str = config::APPLICATION_ID;
const ABOUT_LOGO_SIZE: i32 = 192;
const NOTIFICATION_LIFETIME: Duration = Duration::from_secs(10);
const MAX_EXTERNAL_SLACK_URIS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RealtimePreferencePresentation {
    subtitle: &'static str,
    status_label: &'static str,
    icon_name: &'static str,
    status_css_class: Option<&'static str>,
    show_app_token_row: bool,
}

fn realtime_preference_presentation(status: RealtimeStatus) -> RealtimePreferencePresentation {
    let browser_session = status.transport == Some(RealtimeTransport::BrowserSession);
    let (subtitle, status_label, icon_name, status_css_class) = match status.phase {
        RealtimePhase::Online if browser_session => (
            "Online using XOXC/XOXD",
            "Online",
            "network-wired-symbolic",
            Some("success"),
        ),
        RealtimePhase::Connecting if browser_session => (
            "Connecting using XOXC/XOXD...",
            "Connecting",
            "network-wireless-acquiring-symbolic",
            None,
        ),
        RealtimePhase::Reconnecting if browser_session => (
            "XOXC/XOXD connection interrupted; retrying...",
            "Offline",
            "network-wired-offline-symbolic",
            Some("warning"),
        ),
        RealtimePhase::Online => (
            "Online using Socket Mode",
            "Online",
            "network-wired-symbolic",
            Some("success"),
        ),
        RealtimePhase::Connecting => (
            "Connecting using Socket Mode...",
            "Connecting",
            "network-wireless-acquiring-symbolic",
            None,
        ),
        RealtimePhase::Reconnecting => (
            "Socket Mode connection interrupted; retrying...",
            "Offline",
            "network-wired-offline-symbolic",
            Some("warning"),
        ),
        RealtimePhase::ConfigurationError => (
            "Realtime configuration could not be loaded",
            "Unavailable",
            "dialog-warning-symbolic",
            Some("warning"),
        ),
        RealtimePhase::NotConfigured => (
            "No realtime connection is configured",
            "Not configured",
            "network-wired-offline-symbolic",
            None,
        ),
    };

    RealtimePreferencePresentation {
        subtitle,
        status_label,
        icon_name,
        status_css_class,
        show_app_token_row: !browser_session,
    }
}

fn realtime_group_description(
    status: RealtimeStatus,
    configured_by_environment: bool,
    stored_token: bool,
) -> &'static str {
    if status.transport == Some(RealtimeTransport::BrowserSession) {
        "Uses the imported Slack browser session; no app token is needed."
    } else if status.phase == RealtimePhase::ConfigurationError {
        "Socket Mode configuration could not be loaded. Check the app token and keyring."
    } else if configured_by_environment {
        "Socket Mode is configured by the desktop environment."
    } else if stored_token {
        "Socket Mode is configured. Enter a new xapp- token to replace it."
    } else {
        "Enter an xapp- token with connections:write, then restart Conduit."
    }
}

struct RealtimePreferenceWidgets<'a> {
    group: &'a adw::PreferencesGroup,
    status_row: &'a adw::ActionRow,
    status_label: &'a gtk::Label,
    status_icon: &'a gtk::Image,
    app_token_row: &'a adw::PasswordEntryRow,
}

#[derive(Debug, Clone, Copy)]
struct RealtimePreferenceConfiguration {
    configured_by_environment: bool,
    stored_token: bool,
}

fn update_realtime_preferences(
    window: &ConduitWindow,
    widgets: RealtimePreferenceWidgets<'_>,
    configuration: RealtimePreferenceConfiguration,
) {
    let status = window.realtime_status();
    let presentation = realtime_preference_presentation(status);
    widgets
        .status_row
        .set_subtitle(&gettext(presentation.subtitle));
    widgets
        .status_label
        .set_label(&gettext(presentation.status_label));
    widgets
        .status_icon
        .set_icon_name(Some(presentation.icon_name));
    for class in ["success", "warning", "error"] {
        widgets.status_label.remove_css_class(class);
    }
    if let Some(class) = presentation.status_css_class {
        widgets.status_label.add_css_class(class);
    }
    widgets
        .app_token_row
        .set_visible(presentation.show_app_token_row);
    widgets
        .group
        .set_description(Some(&gettext(realtime_group_description(
            status,
            configuration.configured_by_environment,
            configuration.stored_token,
        ))));
}

fn application_flags() -> gio::ApplicationFlags {
    gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::HANDLES_OPEN
}

fn parse_external_slack_uris<I>(values: I) -> Vec<SlackUri>
where
    I: IntoIterator<Item = String>,
{
    values
        .into_iter()
        .filter_map(|value| parse_slack_uri(&value).ok())
        .take(MAX_EXTERNAL_SLACK_URIS)
        .collect()
}

fn command_line_slack_uris(arguments: Vec<OsString>) -> Vec<SlackUri> {
    parse_external_slack_uris(
        arguments
            .into_iter()
            .skip(1)
            .filter_map(|argument| argument.into_string().ok()),
    )
}

fn record_test_slack_uri_opened() {
    let Some(path) = std::env::var_os("CONDUIT_TEST_OPEN_SLACK_URI_FILE") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "opened");
}

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

fn huddle_notification_id(workspace_id: &str, call_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(workspace_id.as_bytes());
    digest.update([0]);
    digest.update(call_id.as_bytes());
    format!("huddle:{:x}", digest.finalize())
}

fn huddle_notification_content() -> (&'static str, &'static str) {
    ("Slack huddle available", "Open Conduit to view details.")
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

fn thread_target_variant(workspace_id: &str, channel_id: &str, thread_ts: &str) -> glib::Variant {
    (workspace_id, channel_id, thread_ts).to_variant()
}

fn thread_target_from_variant(target: &glib::Variant) -> Option<(String, String, String)> {
    let (workspace_id, channel_id, thread_ts) = target.get::<(String, String, String)>()?;
    let workspace_id = workspace_id.trim();
    let channel_id = channel_id.trim();
    let thread_ts = thread_ts.trim();
    (!workspace_id.is_empty() && !channel_id.is_empty() && !thread_ts.is_empty()).then(|| {
        (
            workspace_id.to_string(),
            channel_id.to_string(),
            thread_ts.to_string(),
        )
    })
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Default)]
    pub struct ConduitApplication {
        search_provider_registration: RefCell<Option<gio::RegistrationId>>,
        debug_enabled: Cell<bool>,
        notification_generations: RefCell<HashMap<String, u64>>,
        next_notification_generation: Cell<u64>,
    }

    impl ConduitApplication {
        pub(super) fn set_debug_enabled(&self, enabled: bool) {
            self.debug_enabled.set(enabled);
        }

        pub(super) fn debug_enabled(&self) -> bool {
            self.debug_enabled.get()
        }

        pub(super) fn register_notification(&self, id: &str) -> u64 {
            let generation = self.next_notification_generation.get().saturating_add(1);
            self.next_notification_generation.set(generation);
            self.notification_generations
                .borrow_mut()
                .insert(id.to_string(), generation);
            generation
        }

        pub(super) fn forget_notification_if_current(&self, id: &str, generation: u64) -> bool {
            let mut generations = self.notification_generations.borrow_mut();
            if generations.get(id) != Some(&generation) {
                return false;
            }
            generations.remove(id);
            true
        }

        pub(super) fn forget_notification(&self, id: &str) {
            self.notification_generations.borrow_mut().remove(id);
        }
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
            crate::debug::set_enabled(self.debug_enabled());
            application.present_window(false, false);
        }

        fn command_line(&self, command_line: &gio::ApplicationCommandLine) -> glib::ExitCode {
            let application = self.obj();
            let options = command_line.options_dict();
            let connect = options.contains("connect");
            let debug = options.contains("debug");
            let debug_auth = debug || options.contains("debug-auth");
            self.set_debug_enabled(debug);
            crate::debug::set_enabled(debug);
            crate::debug::log("app", "debug logging enabled");
            application.present_slack_uris(
                connect,
                debug_auth,
                command_line_slack_uris(command_line.arguments()),
            );
            0.into()
        }

        fn open(&self, files: &[gio::File], _hint: &str) {
            let uris =
                parse_external_slack_uris(files.iter().map(|file| file.uri().as_str().to_string()));
            self.obj().present_slack_uris(false, false, uris);
        }

        fn shutdown(&self) {
            self.obj().flush_active_window_state();
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
    pub fn new(application_id: &str) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", application_flags())
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
        let open_thread_action = gio::ActionEntry::builder("open-thread")
            .parameter_type(Some(
                <(String, String, String)>::static_variant_type().as_ref(),
            ))
            .activate(move |app: &Self, _, target| {
                let Some((workspace_id, channel_id, thread_ts)) =
                    target.and_then(thread_target_from_variant)
                else {
                    return;
                };
                app.present_notification_target(workspace_id, channel_id, Some(thread_ts));
            })
            .build();
        self.add_action_entries([
            quit_action,
            shortcuts_action,
            preferences_action,
            about_action,
            open_conversation_action,
            open_thread_action,
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

    fn present_window(&self, connect: bool, debug_auth: bool) -> ConduitWindow {
        self.configure_icon_theme();

        let window = self
            .active_window()
            .and_then(|window| window.downcast::<ConduitWindow>().ok())
            .unwrap_or_else(|| ConduitWindow::new(self));
        window.set_auth_debug(debug_auth);
        if connect {
            window.show_connect_requested();
        }
        window.present();
        window
    }

    fn present_slack_uris(&self, connect: bool, debug_auth: bool, uris: Vec<SlackUri>) {
        let window = self.present_window(connect, debug_auth);
        for uri in uris {
            if window.open_slack_uri(uri) {
                record_test_slack_uri_opened();
            }
        }
    }

    fn flush_active_window_state(&self) {
        if let Some(window) = self
            .active_window()
            .and_then(|window| window.downcast::<ConduitWindow>().ok())
        {
            window.flush_persistent_state();
        }
    }

    fn present_conversation_target(&self, workspace_id: String, channel_id: String) {
        self.present_notification_target(workspace_id, channel_id, None);
    }

    fn present_notification_target(
        &self,
        workspace_id: String,
        channel_id: String,
        thread_ts: Option<String>,
    ) {
        let window = self.present_window(false, false);
        if window.open_notification_target(
            workspace_id.clone(),
            channel_id.clone(),
            thread_ts.clone(),
        ) {
            if let Some(path) = std::env::var_os("CONDUIT_TEST_OPEN_TARGET_FILE") {
                let mut target = serde_json::json!({
                    "workspace_id": workspace_id,
                    "channel_id": channel_id,
                });
                if let Some(thread_ts) = thread_ts {
                    target["thread_ts"] = serde_json::Value::String(thread_ts);
                }
                let _ = std::fs::write(path, target.to_string());
            }
        }
    }

    pub(crate) fn send_conversation_notification(
        &self,
        workspace_id: &str,
        channel_id: &str,
        title: &str,
        body: &str,
        thread_ts: Option<&str>,
    ) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_priority(gio::NotificationPriority::Normal);
        if let Some(thread_ts) = thread_ts {
            let target = thread_target_variant(workspace_id, channel_id, thread_ts);
            notification.set_default_action_and_target_value(OPEN_THREAD_ACTION, Some(&target));
        } else {
            let target = conversation_target_variant(workspace_id, channel_id);
            notification
                .set_default_action_and_target_value(OPEN_CONVERSATION_ACTION, Some(&target));
        }
        let id = conversation_notification_id(workspace_id, channel_id);
        let generation = self.imp().register_notification(&id);
        self.send_notification(Some(&id), &notification);

        let application = self.downgrade();
        glib::timeout_add_local_once(NOTIFICATION_LIFETIME, move || {
            let Some(application) = application.upgrade() else {
                return;
            };
            if application
                .imp()
                .forget_notification_if_current(&id, generation)
            {
                application.withdraw_notification(&id);
            }
        });
    }

    pub(crate) fn withdraw_conversation_notification(&self, workspace_id: &str, channel_id: &str) {
        let id = conversation_notification_id(workspace_id, channel_id);
        self.imp().forget_notification(&id);
        self.withdraw_notification(&id);
    }

    pub(crate) fn send_huddle_notification(
        &self,
        workspace_id: &str,
        channel_id: &str,
        call_id: &str,
    ) {
        let (title, body) = huddle_notification_content();
        let notification = gio::Notification::new(&gettext(title));
        notification.set_body(Some(&gettext(body)));
        notification.set_priority(gio::NotificationPriority::Normal);
        let target = conversation_target_variant(workspace_id, channel_id);
        notification.set_default_action_and_target_value(OPEN_CONVERSATION_ACTION, Some(&target));
        let id = huddle_notification_id(workspace_id, call_id);
        self.imp().register_notification(&id);
        self.send_notification(Some(&id), &notification);
    }

    pub(crate) fn withdraw_huddle_notification(&self, workspace_id: &str, call_id: &str) {
        let id = huddle_notification_id(workspace_id, call_id);
        self.imp().forget_notification(&id);
        self.withdraw_notification(&id);
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
        let Some(window) = self
            .active_window()
            .and_then(|window| window.downcast::<ConduitWindow>().ok())
        else {
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

        let realtime_row = adw::PasswordEntryRow::builder()
            .title("Socket Mode app token")
            .show_apply_button(true)
            .build();
        let configured_by_environment = config::slack_app_token().is_some();
        realtime_row.set_sensitive(!configured_by_environment);
        let stored_token = (window.realtime_status().transport
            != Some(RealtimeTransport::BrowserSession))
            && AppTokenStore.load().ok().flatten().is_some();
        let realtime_status_row = adw::ActionRow::builder().title("Connection").build();
        let realtime_status_label = gtk::Label::new(None);
        realtime_status_label.set_valign(gtk::Align::Center);
        let realtime_status_icon = gtk::Image::new();
        let realtime_status_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        realtime_status_box.set_valign(gtk::Align::Center);
        realtime_status_box.append(&realtime_status_label);
        realtime_status_box.append(&realtime_status_icon);
        realtime_status_row.add_suffix(&realtime_status_box);

        let realtime_group = adw::PreferencesGroup::builder()
            .title("Realtime updates")
            .build();
        realtime_group.add(&realtime_status_row);
        realtime_group.add(&realtime_row);
        update_realtime_preferences(
            &window,
            RealtimePreferenceWidgets {
                group: &realtime_group,
                status_row: &realtime_status_row,
                status_label: &realtime_status_label,
                status_icon: &realtime_status_icon,
                app_token_row: &realtime_row,
            },
            RealtimePreferenceConfiguration {
                configured_by_environment,
                stored_token,
            },
        );

        let sign_out_row = adw::ActionRow::builder()
            .title("Sign out")
            .subtitle("Remove the stored Slack session from this device.")
            .build();
        let sign_out_button = gtk::Button::with_label("Sign out");
        sign_out_button.add_css_class("destructive-action");
        sign_out_button.set_valign(gtk::Align::Center);
        sign_out_row.add_suffix(&sign_out_button);
        let account_group = adw::PreferencesGroup::builder().title("Account").build();
        account_group.add(&sign_out_row);

        let page = adw::PreferencesPage::builder()
            .title("Preferences")
            .icon_name("view-list-symbolic")
            .build();
        page.add(&sidebar_group);
        page.add(&realtime_group);
        page.add(&account_group);

        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .build();
        let realtime_group_for_status = realtime_group.clone();
        let realtime_status_row_for_status = realtime_status_row.clone();
        let realtime_status_label_for_status = realtime_status_label.clone();
        let realtime_status_icon_for_status = realtime_status_icon.clone();
        let realtime_row_for_status = realtime_row.clone();
        let status_handler = Rc::new(RefCell::new(Some(window.connect_realtime_status_changed(
            move |window| {
                update_realtime_preferences(
                    window,
                    RealtimePreferenceWidgets {
                        group: &realtime_group_for_status,
                        status_row: &realtime_status_row_for_status,
                        status_label: &realtime_status_label_for_status,
                        status_icon: &realtime_status_icon_for_status,
                        app_token_row: &realtime_row_for_status,
                    },
                    RealtimePreferenceConfiguration {
                        configured_by_environment,
                        stored_token,
                    },
                );
            },
        ))));
        let weak_status_window = window.downgrade();
        let status_handler_on_close = Rc::clone(&status_handler);
        dialog.connect_closed(move |_| {
            if let (Some(window), Some(handler)) = (
                weak_status_window.upgrade(),
                status_handler_on_close.borrow_mut().take(),
            ) {
                window.disconnect(handler);
            }
        });
        let weak_dialog = dialog.downgrade();
        realtime_row.connect_apply(move |row| {
            let result = AppTokenStore.save(row.text().as_str());
            let Some(dialog) = weak_dialog.upgrade() else {
                return;
            };
            match result {
                Ok(()) => {
                    row.set_text("");
                    dialog.add_toast(adw::Toast::new(
                        "App token saved. Restart Conduit to enable realtime updates.",
                    ));
                }
                Err(error) => dialog.add_toast(adw::Toast::new(&error.to_string())),
            }
        });
        let weak_dialog = dialog.downgrade();
        let weak_window = window.downgrade();
        sign_out_button.connect_clicked(move |_| {
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.close();
            }
            if let Some(window) = weak_window.upgrade() {
                let _ = gtk::prelude::WidgetExt::activate_action(&window, "win.sign-out", None);
            }
        });
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
    fn browser_session_realtime_status_hides_the_app_token_editor() {
        for phase in [
            RealtimePhase::Connecting,
            RealtimePhase::Online,
            RealtimePhase::Reconnecting,
        ] {
            let presentation = realtime_preference_presentation(RealtimeStatus {
                transport: Some(RealtimeTransport::BrowserSession),
                phase,
            });
            assert!(!presentation.show_app_token_row);
        }

        let online = realtime_preference_presentation(RealtimeStatus::online(
            RealtimeTransport::BrowserSession,
        ));
        assert_eq!(online.status_label, "Online");
        assert_eq!(online.subtitle, "Online using XOXC/XOXD");

        let reconnecting = realtime_preference_presentation(RealtimeStatus::reconnecting(
            RealtimeTransport::BrowserSession,
        ));
        assert_eq!(reconnecting.status_label, "Offline");
        assert!(reconnecting.subtitle.contains("retrying"));
    }

    #[test]
    fn socket_mode_and_unconfigured_realtime_keep_the_app_token_editor() {
        assert!(realtime_preference_presentation(RealtimeStatus::default()).show_app_token_row);
        assert!(
            realtime_preference_presentation(RealtimeStatus::online(RealtimeTransport::SocketMode))
                .show_app_token_row
        );
        assert!(
            realtime_group_description(RealtimeStatus::configuration_error(), true, false)
                .contains("could not be loaded")
        );
    }

    #[test]
    fn requested_debug_mode_survives_application_activation() {
        let state = imp::ConduitApplication::default();

        assert!(!state.debug_enabled());
        state.set_debug_enabled(true);
        assert!(state.debug_enabled());
    }

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
    fn huddle_notification_ids_and_content_do_not_expose_private_details() {
        let id = huddle_notification_id("T123:U123", "R456");
        let (title, body) = huddle_notification_content();

        assert_eq!(id, huddle_notification_id("T123:U123", "R456"));
        assert_ne!(id, huddle_notification_id("T123:U123", "R999"));
        assert!(id.starts_with("huddle:"));
        assert!(!id.contains("T123"));
        assert!(!id.contains("R456"));
        assert_eq!(title, "Slack huddle available");
        assert_eq!(body, "Open Conduit to view details.");
        assert!(!body.contains("participant"));
    }

    #[test]
    fn notification_expiry_does_not_withdraw_a_newer_replacement() {
        let state = imp::ConduitApplication::default();
        let first = state.register_notification("message:1");
        let second = state.register_notification("message:1");

        assert_eq!(NOTIFICATION_LIFETIME, Duration::from_secs(10));
        assert!(!state.forget_notification_if_current("message:1", first));
        assert!(state.forget_notification_if_current("message:1", second));
        assert!(!state.forget_notification_if_current("message:1", second));

        let manually_withdrawn = state.register_notification("message:2");
        state.forget_notification("message:2");
        assert!(!state.forget_notification_if_current("message:2", manually_withdrawn));
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

        let thread_target = thread_target_variant("T123", "C123", "1710000000.000100");
        assert_eq!(
            thread_target_from_variant(&thread_target),
            Some(("T123".into(), "C123".into(), "1710000000.000100".into()))
        );
        assert_eq!(
            thread_target_from_variant(&thread_target_variant("T123", "C123", "  ")),
            None
        );
    }

    #[test]
    fn command_line_extracts_only_valid_slack_uris() {
        let uris = command_line_slack_uris(vec![
            OsString::from("conduit"),
            OsString::from("--debug"),
            OsString::from("https://example.slack.com/archives/C456"),
            OsString::from("slack://channel?team=T123&id=C456"),
            OsString::from("slack://channel?team=T123"),
            OsString::from("slack://user?team=T123&id=U456"),
        ]);

        assert_eq!(
            uris,
            vec![
                crate::slack_link::parse_slack_uri("slack://channel?team=T123&id=C456").unwrap(),
                crate::slack_link::parse_slack_uri("slack://user?team=T123&id=U456").unwrap(),
            ]
        );
    }

    #[test]
    fn application_accepts_command_line_and_dbus_open_requests() {
        let flags = application_flags();

        assert!(flags.contains(gio::ApplicationFlags::HANDLES_COMMAND_LINE));
        assert!(flags.contains(gio::ApplicationFlags::HANDLES_OPEN));
    }

    #[test]
    fn external_slack_uri_batches_are_bounded() {
        let arguments = std::iter::once(OsString::from("conduit"))
            .chain((0..20).map(|_| OsString::from("slack://open")))
            .collect();

        assert_eq!(
            command_line_slack_uris(arguments).len(),
            MAX_EXTERNAL_SLACK_URIS
        );
    }
}
