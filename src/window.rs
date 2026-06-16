/* window.rs
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

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use adw::subclass::prelude::*;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::models::{AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackMessage};
use crate::runtime::{AppRuntime, RuntimeCommand, RuntimeEvent};

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/eu/vanadrighem/conduit/window.ui")]
    pub struct ConduitWindow {
        #[template_child]
        pub content_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub client_id_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub connect_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub connection_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub conversation_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub message_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub message_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upload_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub search_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub saved_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub refresh_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub sign_out_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub thread_pane: TemplateChild<gtk::Box>,
        #[template_child]
        pub thread_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub thread_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub thread_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub thread_send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub close_thread_button: TemplateChild<gtk::Button>,

        pub runtime: RefCell<Option<AppRuntime>>,
        pub events: RefCell<Option<Receiver<RuntimeEvent>>>,
        pub conversations: RefCell<Vec<SlackConversation>>,
        pub selected_channel: RefCell<Option<String>>,
        pub selected_thread_ts: RefCell<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ConduitWindow {
        const NAME: &'static str = "ConduitWindow";
        type Type = super::ConduitWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ConduitWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_runtime();
            obj.setup_callbacks();
            obj.show_loading("Checking secure storage");
        }
    }

    impl WidgetImpl for ConduitWindow {}
    impl WindowImpl for ConduitWindow {}
    impl ApplicationWindowImpl for ConduitWindow {}
    impl AdwApplicationWindowImpl for ConduitWindow {}
}

glib::wrapper! {
    pub struct ConduitWindow(ObjectSubclass<imp::ConduitWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl ConduitWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    fn setup_runtime(&self) {
        let imp = self.imp();
        let (sender, receiver) = mpsc::channel::<RuntimeEvent>();
        let runtime = AppRuntime::start(sender);

        *imp.runtime.borrow_mut() = Some(runtime.clone());
        *imp.events.borrow_mut() = Some(receiver);

        let weak_window = self.downgrade();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let Some(window) = weak_window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            window.drain_runtime_events();
            glib::ControlFlow::Continue
        });

        runtime.send(RuntimeCommand::LoadStoredToken);
    }

    fn setup_callbacks(&self) {
        let imp = self.imp();

        self.connect_widget(&imp.connect_button.get(), |window| window.start_oauth());
        self.connect_widget(&imp.refresh_button.get(), |window| {
            window.send_command(RuntimeCommand::RefreshConversations)
        });
        self.connect_widget(&imp.saved_button.get(), |window| {
            window.send_command(RuntimeCommand::LoadSavedItems)
        });
        self.connect_widget(&imp.sign_out_button.get(), |window| {
            window.send_command(RuntimeCommand::SignOut)
        });
        self.connect_widget(&imp.search_button.get(), |window| window.search_messages());
        self.connect_widget(&imp.send_button.get(), |window| {
            window.post_current_message()
        });
        self.connect_widget(&imp.upload_button.get(), |window| {
            window.choose_file_for_upload()
        });
        self.connect_widget(&imp.thread_send_button.get(), |window| {
            window.post_thread_reply()
        });
        self.connect_widget(&imp.close_thread_button.get(), |window| {
            window.close_thread()
        });

        let weak_window = self.downgrade();
        imp.message_entry.connect_activate(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.post_current_message();
            }
        });

        let weak_window = self.downgrade();
        imp.thread_entry.connect_activate(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.post_thread_reply();
            }
        });

        let weak_window = self.downgrade();
        imp.search_entry.connect_activate(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.search_messages();
            }
        });
    }

    fn connect_widget<W, F>(&self, widget: &W, callback: F)
    where
        W: IsA<gtk::Button>,
        F: Fn(&Self) + 'static,
    {
        let weak_window = self.downgrade();
        widget.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                callback(&window);
            }
        });
    }

    fn drain_runtime_events(&self) {
        loop {
            let event = {
                let imp = self.imp();
                let events = imp.events.borrow();
                let Some(receiver) = events.as_ref() else {
                    return;
                };
                receiver.try_recv()
            };

            match event {
                Ok(event) => self.handle_runtime_event(event),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.show_error("Background runtime stopped");
                    return;
                }
            }
        }
    }

    fn handle_runtime_event(&self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Status(status) => self.set_status(&status),
            RuntimeEvent::Error(error) => self.show_error(&error),
            RuntimeEvent::SignedOut => self.show_login("Signed out"),
            RuntimeEvent::Authenticated(auth) => self.show_workspace(auth),
            RuntimeEvent::ConversationsLoaded(conversations) => {
                self.populate_conversations(conversations);
            }
            RuntimeEvent::HistoryLoaded {
                channel_id,
                messages,
            } => {
                self.populate_history(&channel_id, messages);
            }
            RuntimeEvent::ThreadLoaded {
                channel_id,
                ts,
                messages,
            } => {
                self.populate_thread(&channel_id, &ts, messages);
            }
            RuntimeEvent::SearchLoaded(results) => self.populate_search_results(results),
            RuntimeEvent::SavedItemsLoaded(items) => self.populate_saved_items(items),
            RuntimeEvent::MessagePosted {
                channel_id,
                message,
            } => {
                self.imp().message_entry.set_text("");
                self.reload_after_message(&channel_id, message.thread_ts.as_deref());
            }
            RuntimeEvent::FileUploaded(name) => {
                self.set_status(&format!("Uploaded {name}"));
                if let Some(channel_id) = self.imp().selected_channel.borrow().clone() {
                    self.send_command(RuntimeCommand::LoadHistory { channel_id });
                }
            }
        }
    }

    fn start_oauth(&self) {
        let client_id = self.imp().client_id_entry.text().trim().to_string();
        if client_id.is_empty() {
            self.show_login("Enter a Slack client ID");
            return;
        }

        self.show_loading("Opening Slack authorization");
        self.send_command(RuntimeCommand::StartOAuth { client_id });
    }

    fn search_messages(&self) {
        let query = self.imp().search_entry.text().trim().to_string();
        if query.is_empty() {
            self.set_status("Enter a search query");
            return;
        }
        self.imp().message_title.set_label("Search results");
        self.clear_list(&self.imp().message_list);
        self.append_placeholder(&self.imp().message_list, "Searching");
        self.send_command(RuntimeCommand::SearchMessages { query });
    }

    fn post_current_message(&self) {
        let imp = self.imp();
        let Some(channel_id) = imp.selected_channel.borrow().clone() else {
            self.set_status("Select a conversation");
            return;
        };
        let text = imp.message_entry.text().trim().to_string();
        if text.is_empty() {
            return;
        }

        self.send_command(RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts: None,
        });
    }

    fn post_thread_reply(&self) {
        let imp = self.imp();
        let Some(channel_id) = imp.selected_channel.borrow().clone() else {
            self.set_status("Select a conversation");
            return;
        };
        let Some(thread_ts) = imp.selected_thread_ts.borrow().clone() else {
            self.set_status("Open a thread");
            return;
        };
        let text = imp.thread_entry.text().trim().to_string();
        if text.is_empty() {
            return;
        }

        self.send_command(RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts: Some(thread_ts),
        });
    }

    fn choose_file_for_upload(&self) {
        let Some(channel_id) = self.imp().selected_channel.borrow().clone() else {
            self.set_status("Select a conversation");
            return;
        };

        let dialog = gtk::FileDialog::builder()
            .title("Upload File")
            .accept_label("Upload")
            .modal(true)
            .build();

        let weak_window = self.downgrade();
        dialog.open(Some(self), None::<&gio::Cancellable>, move |result| {
            if let Ok(file) = result {
                if let Some(path) = file.path() {
                    if let Some(window) = weak_window.upgrade() {
                        window.send_command(RuntimeCommand::UploadFile {
                            channel_id: channel_id.clone(),
                            path,
                        });
                    }
                }
            }
        });
    }

    fn close_thread(&self) {
        let imp = self.imp();
        *imp.selected_thread_ts.borrow_mut() = None;
        imp.thread_entry.set_text("");
        imp.thread_pane.set_visible(false);
        self.clear_list(&imp.thread_list);
    }

    fn reload_after_message(&self, channel_id: &str, thread_ts: Option<&str>) {
        if let Some(thread_ts) = thread_ts {
            self.imp().thread_entry.set_text("");
            self.send_command(RuntimeCommand::LoadThread {
                channel_id: channel_id.to_string(),
                ts: thread_ts.to_string(),
            });
        } else {
            self.send_command(RuntimeCommand::LoadHistory {
                channel_id: channel_id.to_string(),
            });
        }
    }

    fn show_loading(&self, status: &str) {
        let imp = self.imp();
        imp.status_label.set_label(status);
        imp.content_stack.set_visible_child_name("loading");
    }

    fn show_login(&self, status: &str) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = None;
        *imp.selected_thread_ts.borrow_mut() = None;
        imp.connection_label.set_label(status);
        imp.content_stack.set_visible_child_name("login");
        self.clear_list(&imp.conversation_list);
        self.clear_list(&imp.message_list);
        self.clear_list(&imp.thread_list);
    }

    fn show_workspace(&self, auth: AuthInfo) {
        let label = auth
            .team
            .or(auth.team_id)
            .map(|team| format!("Connected to {team}"))
            .unwrap_or_else(|| "Connected to Slack".to_string());
        self.set_status(&label);
        self.imp().content_stack.set_visible_child_name("workspace");
    }

    fn set_status(&self, status: &str) {
        let imp = self.imp();
        imp.status_label.set_label(status);
        imp.connection_label.set_label(status);
    }

    fn show_error(&self, error: &str) {
        self.set_status(error);
        if self.imp().content_stack.visible_child_name().as_deref() == Some("loading") {
            self.imp().content_stack.set_visible_child_name("login");
        }
    }

    fn populate_conversations(&self, conversations: Vec<SlackConversation>) {
        let imp = self.imp();
        *imp.conversations.borrow_mut() = conversations.clone();
        self.clear_list(&imp.conversation_list);

        if conversations.is_empty() {
            self.append_placeholder(&imp.conversation_list, "No conversations");
            return;
        }

        for conversation in conversations {
            let row = gtk::ListBoxRow::new();
            let button = gtk::Button::with_label(&conversation.display_name());
            button.set_halign(gtk::Align::Fill);
            button.set_hexpand(true);
            button.add_css_class("flat");

            let channel_id = conversation.id.clone();
            let title = conversation.display_name();
            let weak_window = self.downgrade();
            button.connect_clicked(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.select_conversation(&channel_id, &title);
                }
            });

            row.set_child(Some(&button));
            imp.conversation_list.append(&row);
        }
    }

    fn select_conversation(&self, channel_id: &str, title: &str) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        *imp.selected_thread_ts.borrow_mut() = None;
        imp.message_title.set_label(title);
        imp.thread_pane.set_visible(false);
        self.clear_list(&imp.message_list);
        self.append_placeholder(&imp.message_list, "Loading messages");
        self.send_command(RuntimeCommand::LoadHistory {
            channel_id: channel_id.to_string(),
        });
    }

    fn populate_history(&self, channel_id: &str, messages: Vec<SlackMessage>) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        self.clear_list(&imp.message_list);

        if messages.is_empty() {
            self.append_placeholder(&imp.message_list, "No messages");
            return;
        }

        for message in messages.into_iter().rev() {
            let row = self.message_row(channel_id, &message, false);
            imp.message_list.append(&row);
        }
    }

    fn populate_thread(&self, channel_id: &str, ts: &str, messages: Vec<SlackMessage>) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        *imp.selected_thread_ts.borrow_mut() = Some(ts.to_string());
        imp.thread_title.set_label("Thread");
        imp.thread_pane.set_visible(true);
        self.clear_list(&imp.thread_list);

        if messages.is_empty() {
            self.append_placeholder(&imp.thread_list, "No replies");
            return;
        }

        for message in messages {
            let row = self.message_row(channel_id, &message, true);
            imp.thread_list.append(&row);
        }
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_label("Search results");
        self.clear_list(&imp.message_list);

        if results.is_empty() {
            self.append_placeholder(&imp.message_list, "No results");
            return;
        }

        for result in results {
            let row = gtk::ListBoxRow::new();
            let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
            container.set_margin_top(8);
            container.set_margin_bottom(8);
            container.set_margin_start(8);
            container.set_margin_end(8);

            let channel = result
                .channel
                .as_ref()
                .and_then(|channel| channel.name.clone())
                .unwrap_or_else(|| "Slack".to_string());
            let author = result
                .username
                .or(result.user)
                .unwrap_or_else(|| "Unknown".to_string());
            let heading = gtk::Label::new(Some(&format!("#{channel} - {author}")));
            heading.set_xalign(0.0);
            heading.add_css_class("caption");
            container.append(&heading);

            let body = gtk::Label::new(Some(result.text.as_deref().unwrap_or_default()));
            body.set_xalign(0.0);
            body.set_wrap(true);
            body.set_selectable(true);
            container.append(&body);

            row.set_child(Some(&container));
            imp.message_list.append(&row);
        }
    }

    fn populate_saved_items(&self, items: Vec<SavedItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Saved items");
        self.clear_list(&imp.message_list);

        if items.is_empty() {
            self.append_placeholder(&imp.message_list, "No saved items");
            return;
        }

        for item in items {
            if let (Some(channel_id), Some(message)) = (item.channel, item.message) {
                let row = self.message_row(&channel_id, &message, false);
                imp.message_list.append(&row);
            }
        }
    }

    fn message_row(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        in_thread: bool,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
        container.set_margin_top(10);
        container.set_margin_bottom(10);
        container.set_margin_start(10);
        container.set_margin_end(10);

        let heading = gtk::Label::new(Some(&message.author_label()));
        heading.set_xalign(0.0);
        heading.add_css_class("caption");
        container.append(&heading);

        let body = gtk::Label::new(Some(&message.body_text()));
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_selectable(true);
        container.append(&body);

        if let Some(files) = message.files.as_ref() {
            for file in files {
                let label = file
                    .title
                    .as_ref()
                    .or(file.name.as_ref())
                    .or(file.id.as_ref())
                    .map(String::as_str)
                    .unwrap_or("File");
                let file_label = gtk::Label::new(Some(&format!("Attachment: {label}")));
                file_label.set_xalign(0.0);
                file_label.add_css_class("caption");
                container.append(&file_label);
            }
        }

        if message.has_thread() && !in_thread {
            let button = gtk::Button::with_label("View thread");
            button.set_halign(gtk::Align::Start);
            let channel_id = channel_id.to_string();
            let ts = message.ts.clone();
            let weak_window = self.downgrade();
            button.connect_clicked(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.send_command(RuntimeCommand::LoadThread {
                        channel_id: channel_id.clone(),
                        ts: ts.clone(),
                    });
                }
            });
            container.append(&button);
        }

        row.set_child(Some(&container));
        row
    }

    fn clear_list(&self, list: &gtk::ListBox) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
    }

    fn append_placeholder(&self, list: &gtk::ListBox, text: &str) {
        let row = gtk::ListBoxRow::new();
        let label = gtk::Label::new(Some(text));
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_xalign(0.0);
        row.set_child(Some(&label));
        list.append(&row);
    }

    fn send_command(&self, command: RuntimeCommand) {
        if let Some(runtime) = self.imp().runtime.borrow().as_ref() {
            runtime.send(command);
        }
    }
}
