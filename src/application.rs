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
use gtk::{gio, glib};

use crate::config::VERSION;
use crate::ConduitWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct ConduitApplication {}

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
            obj.set_accels_for_action("app.quit", &["<control>q"]);
        }
    }

    impl ApplicationImpl for ConduitApplication {
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
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit_action, shortcuts_action, about_action]);
        self.set_accels_for_action("app.shortcuts", &["<control>question"]);
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

    fn show_about(&self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let about = adw::AboutDialog::builder()
            .application_name("Conduit")
            .application_icon("eu.vanadrighem.conduit-about")
            .developer_name("Vincent van Adrighem")
            .version(VERSION)
            .developers(vec!["Vincent van Adrighem"])
            // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
            .translator_credits(gettext("translator-credits"))
            .copyright("© 2026 Vincent van Adrighem")
            .build();

        about.present(Some(&window));
    }
}
