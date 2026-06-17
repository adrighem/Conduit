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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::auth;
use crate::config;
use crate::models::{SavedItem, SearchMatch, SlackConversation, SlackMessage};
use crate::rendering;
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
        pub setup_hint_label: TemplateChild<gtk::Label>,
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
        pub upload_progress: TemplateChild<gtk::ProgressBar>,
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
        pub connect_requested: Cell<bool>,
        pub conversations: RefCell<Vec<SlackConversation>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub current_channel_messages: RefCell<Vec<SlackMessage>>,
        pub current_thread_messages: RefCell<Vec<SlackMessage>>,
        pub current_user_id: RefCell<Option<String>>,
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
            obj.configure_auth_ui();
            obj.setup_callbacks();
            obj.show_loading("Checking secure storage");
            obj.send_command(RuntimeCommand::LoadStoredToken);
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
            RuntimeEvent::Status(status) => {
                if !self.imp().connect_requested.get() {
                    self.set_status(&status);
                }
            }
            RuntimeEvent::Error(error) => self.show_error(&error),
            RuntimeEvent::SignedOut => {
                self.imp().connect_requested.set(false);
                self.show_login("Choose a workspace to continue");
            }
            RuntimeEvent::Authenticated(auth) => {
                if !self.imp().connect_requested.get() {
                    self.show_workspace(auth);
                }
            }
            RuntimeEvent::ConversationsLoaded(conversations) => {
                if !self.imp().connect_requested.get() {
                    self.populate_conversations(conversations);
                }
            }
            RuntimeEvent::HistoryLoaded {
                channel_id,
                messages,
            } => {
                self.notify_if_new_messages(&channel_id, &messages);
                self.request_user_names(&messages);
                *self.imp().current_channel_messages.borrow_mut() = messages.clone();
                self.populate_history(&channel_id, messages);
            }
            RuntimeEvent::ThreadLoaded {
                channel_id,
                ts,
                messages,
            } => {
                self.request_user_names(&messages);
                *self.imp().current_thread_messages.borrow_mut() = messages.clone();
                self.populate_thread(&channel_id, &ts, messages);
            }
            RuntimeEvent::SearchLoaded(results) => self.populate_search_results(results),
            RuntimeEvent::SavedItemsLoaded(items) => self.populate_saved_items(items),
            RuntimeEvent::UserLoaded {
                user_id,
                display_name,
            } => {
                self.imp()
                    .user_names
                    .borrow_mut()
                    .insert(user_id, display_name);
                self.rerender_current_messages();
            }
            RuntimeEvent::MessagePosted {
                channel_id,
                message,
            } => {
                self.imp().message_entry.set_text("");
                self.imp().send_button.set_sensitive(true);
                self.imp().thread_send_button.set_sensitive(true);
                self.set_status("Message sent");
                self.reload_after_message(&channel_id, message.thread_ts.as_deref());
            }
            RuntimeEvent::ReactionUpdated {
                channel_id,
                thread_ts,
            } => {
                self.set_status("Reaction updated");
                self.reload_after_message(&channel_id, thread_ts.as_deref());
            }
            RuntimeEvent::FileUploadProgress { fraction, label } => {
                let imp = self.imp();
                imp.upload_progress.set_visible(true);
                imp.upload_progress.set_fraction(fraction);
                imp.upload_progress.set_text(Some(&label));
                self.set_status(&label);
            }
            RuntimeEvent::FileUploaded(name) => {
                let imp = self.imp();
                imp.upload_button.set_sensitive(true);
                imp.upload_progress.set_fraction(1.0);
                imp.upload_progress.set_text(Some("Upload complete"));
                imp.message_entry.set_text("");
                self.set_status(&format!("Uploaded {name}"));
                if let Some(channel_id) = self.imp().selected_channel.borrow().clone() {
                    self.send_command(RuntimeCommand::LoadHistory { channel_id });
                }
            }
        }
    }

    fn configure_auth_ui(&self) {
        let imp = self.imp();
        if let Some(client_id) = config::slack_client_id() {
            imp.client_id_entry.set_text(&client_id);
            imp.client_id_entry.set_visible(false);
            imp.setup_hint_label.set_visible(false);
        } else {
            imp.setup_hint_label.set_label(&format!(
                "Use redirect URL {} in the Slack app settings.",
                auth::OAuthConfig::new("").redirect_uri()
            ));
        }
    }

    fn start_oauth(&self) {
        let client_id = self.imp().client_id_entry.text().trim().to_string();
        if client_id.is_empty() {
            self.show_login("Enter a Slack app client ID");
            return;
        }

        self.imp().connect_requested.set(false);
        self.imp().connect_button.set_sensitive(false);
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
        imp.send_button.set_sensitive(false);
        self.set_status("Sending message");
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
        imp.thread_send_button.set_sensitive(false);
        self.set_status("Sending reply");
    }

    fn choose_file_for_upload(&self) {
        let Some(channel_id) = self.imp().selected_channel.borrow().clone() else {
            self.set_status("Select a conversation");
            return;
        };
        let initial_comment = self.imp().message_entry.text().trim().to_string();

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
                        let imp = window.imp();
                        imp.upload_button.set_sensitive(false);
                        imp.upload_progress.set_visible(true);
                        imp.upload_progress.set_fraction(0.0);
                        imp.upload_progress.set_text(Some("Starting upload"));
                        window.send_command(RuntimeCommand::UploadFile {
                            channel_id: channel_id.clone(),
                            path,
                            initial_comment: (!initial_comment.is_empty())
                                .then(|| initial_comment.clone()),
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
        imp.connection_label.set_label(status);
        imp.content_stack.set_visible_child_name("loading");
    }

    fn show_login(&self, status: &str) {
        let imp = self.imp();
        self.reset_workspace_state();
        imp.connect_button.set_sensitive(true);
        imp.connection_label.set_label(status);
        imp.content_stack.set_visible_child_name("connect");
    }

    pub(crate) fn show_connect_requested(&self) {
        self.imp().connect_requested.set(true);
        self.show_login("Choose a workspace to continue");
    }

    fn reset_workspace_state(&self) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = None;
        *imp.selected_thread_ts.borrow_mut() = None;
        *imp.current_user_id.borrow_mut() = None;
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.user_names.borrow_mut().clear();
        imp.current_channel_messages.borrow_mut().clear();
        imp.current_thread_messages.borrow_mut().clear();
        self.clear_list(&imp.conversation_list);
        self.clear_list(&imp.message_list);
        self.clear_list(&imp.thread_list);
    }

    fn show_workspace(&self, auth: crate::models::AuthInfo) {
        *self.imp().current_user_id.borrow_mut() = auth.user_id.clone();
        self.imp().connect_button.set_sensitive(true);
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
        self.imp().send_button.set_sensitive(true);
        self.imp().thread_send_button.set_sensitive(true);
        self.imp().upload_button.set_sensitive(true);
        self.imp().upload_progress.set_visible(false);
        if self.imp().content_stack.visible_child_name().as_deref() == Some("loading") {
            self.show_login(error);
        } else {
            self.set_status(error);
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

            let body = rendering::rich_label(
                result.text.as_deref().unwrap_or_default(),
                &self.imp().user_names.borrow(),
            );
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

        let heading = gtk::Label::new(Some(&self.message_author_label(message)));
        heading.set_xalign(0.0);
        heading.add_css_class("caption");
        container.append(&heading);

        rendering::append_message_content(&container, message, &self.imp().user_names.borrow());

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

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let current_user_id = self.imp().current_user_id.borrow().clone();
        let reacted = message.user_reacted("thumbsup", current_user_id.as_deref());
        let reaction_button = gtk::Button::with_label(if reacted { "Remove +1" } else { "+1" });
        reaction_button.set_halign(gtk::Align::Start);
        let reaction_channel_id = channel_id.to_string();
        let reaction_ts = message.ts.clone();
        let reaction_thread_ts = if in_thread {
            self.imp().selected_thread_ts.borrow().clone()
        } else {
            message.thread_ts.clone()
        };
        let weak_window = self.downgrade();
        reaction_button.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.send_command(RuntimeCommand::SetReaction {
                    channel_id: reaction_channel_id.clone(),
                    ts: reaction_ts.clone(),
                    name: "thumbsup".to_string(),
                    add: !reacted,
                    thread_ts: reaction_thread_ts.clone(),
                });
                window.set_status(if reacted {
                    "Removing reaction"
                } else {
                    "Adding reaction"
                });
            }
        });
        actions.append(&reaction_button);

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
            actions.append(&button);
        }

        container.append(&actions);

        row.set_child(Some(&container));
        row
    }

    fn notify_if_new_messages(&self, channel_id: &str, messages: &[SlackMessage]) {
        let Some(latest_ts) = SlackMessage::latest_ts(messages.iter()) else {
            return;
        };

        let latest_message = messages
            .iter()
            .filter(|message| message.ts == latest_ts)
            .next();
        let current_user_id = self.imp().current_user_id.borrow().clone();

        if latest_message
            .and_then(|message| message.user.as_deref())
            .is_some_and(|user| Some(user) == current_user_id.as_deref())
        {
            self.imp()
                .latest_message_ts_by_channel
                .borrow_mut()
                .insert(channel_id.to_string(), latest_ts);
            return;
        }

        let previous_ts = self
            .imp()
            .latest_message_ts_by_channel
            .borrow()
            .get(channel_id)
            .cloned();

        self.imp()
            .latest_message_ts_by_channel
            .borrow_mut()
            .insert(channel_id.to_string(), latest_ts.clone());

        if previous_ts
            .as_deref()
            .is_some_and(|previous_ts| latest_ts.as_str() > previous_ts)
        {
            self.send_notification(
                &self.conversation_title(channel_id),
                latest_message
                    .map(SlackMessage::body_text)
                    .as_deref()
                    .unwrap_or("New message"),
            );
        }
    }

    fn send_notification(&self, title: &str, body: &str) {
        let Some(application) = self.application() else {
            return;
        };

        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        application.send_notification(None, &notification);
    }

    fn conversation_title(&self, channel_id: &str) -> String {
        self.imp()
            .conversations
            .borrow()
            .iter()
            .find(|conversation| conversation.id == channel_id)
            .map(SlackConversation::display_name)
            .unwrap_or_else(|| "Slack".to_string())
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

    fn request_user_names(&self, messages: &[SlackMessage]) {
        let mut ids = messages
            .iter()
            .flat_map(rendering::extract_user_ids)
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();

        for user_id in ids {
            if !self.imp().user_names.borrow().contains_key(&user_id) {
                self.send_command(RuntimeCommand::LoadUser { user_id });
            }
        }
    }

    fn message_author_label(&self, message: &SlackMessage) -> String {
        if let Some(user_id) = message.user.as_ref() {
            if let Some(name) = self.imp().user_names.borrow().get(user_id) {
                return name.clone();
            }
        }

        message.author_label()
    }

    fn rerender_current_messages(&self) {
        let imp = self.imp();
        if let Some(channel_id) = imp.selected_channel.borrow().clone() {
            let messages = imp.current_channel_messages.borrow().clone();
            if !messages.is_empty() {
                self.populate_history(&channel_id, messages);
            }

            if let Some(thread_ts) = imp.selected_thread_ts.borrow().clone() {
                let thread_messages = imp.current_thread_messages.borrow().clone();
                if !thread_messages.is_empty() {
                    self.populate_thread(&channel_id, &thread_ts, thread_messages);
                }
            }
        }
    }
}
