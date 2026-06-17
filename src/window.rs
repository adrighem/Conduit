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
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::auth;
use crate::config;
use crate::message_html::{self, MessageHtmlContext};
use crate::models::{SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage};
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
        pub message_view_box: TemplateChild<gtk::Box>,
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
        pub auth_debug: Cell<bool>,
        pub conversations: RefCell<Vec<SlackConversation>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub current_channel_messages: RefCell<Vec<SlackMessage>>,
        pub current_thread_messages: RefCell<Vec<SlackMessage>>,
        pub current_search_results: RefCell<Vec<SearchMatch>>,
        pub current_saved_items: RefCell<Vec<SavedItem>>,
        pub current_main_view: Cell<MainMessageView>,
        pub current_user_id: RefCell<Option<String>>,
        pub selected_channel: RefCell<Option<String>>,
        pub selected_thread_ts: RefCell<Option<String>>,
        pub message_view: RefCell<Option<webkit6::WebView>>,
        pub image_assets: RefCell<HashMap<String, String>>,
        pub pending_image_assets: RefCell<HashSet<String>>,
        pub failed_image_assets: RefCell<HashSet<String>>,
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
            obj.setup_message_view();
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

#[derive(Debug, Clone, Default)]
struct MessageRenderContext {
    user_names: HashMap<String, String>,
    current_user_id: Option<String>,
    selected_thread_ts: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CurrentMessageSnapshot {
    channel_id: Option<String>,
    thread_ts: Option<String>,
    channel_messages: Vec<SlackMessage>,
    thread_messages: Vec<SlackMessage>,
    search_results: Vec<SearchMatch>,
    saved_items: Vec<SavedItem>,
    main_view: MainMessageView,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MainMessageView {
    #[default]
    Placeholder,
    Conversation,
    Search,
    Saved,
}

fn message_navigation_uri(decision: &webkit6::PolicyDecision) -> Option<String> {
    let navigation = decision.downcast_ref::<webkit6::NavigationPolicyDecision>()?;
    let mut action = navigation.navigation_action()?;
    let request = action.request()?;
    request.uri().map(|uri| uri.to_string())
}

fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn image_asset_request(file: &SlackFile) -> Option<(String, String)> {
    let url = file.preview_url()?;
    Some((url.to_string(), url.to_string()))
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

    fn setup_message_view(&self) {
        let settings = webkit6::Settings::new();
        settings.set_allow_file_access_from_file_urls(false);
        settings.set_allow_universal_access_from_file_urls(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_javascript(false);
        settings.set_enable_media(false);
        settings.set_enable_webgl(false);
        settings.set_enable_webaudio(false);

        let network_session = webkit6::NetworkSession::new_ephemeral();
        let web_view = webkit6::WebView::builder()
            .network_session(&network_session)
            .settings(&settings)
            .build();
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);

        let weak_window = self.downgrade();
        web_view.connect_decide_policy(move |_, decision, decision_type| {
            if !matches!(
                decision_type,
                webkit6::PolicyDecisionType::NavigationAction
                    | webkit6::PolicyDecisionType::NewWindowAction
            ) {
                return false;
            }

            let Some(uri) = message_navigation_uri(decision) else {
                return false;
            };

            let handled = weak_window
                .upgrade()
                .is_some_and(|window| window.handle_message_view_uri(&uri));
            if handled {
                decision.ignore();
            }
            handled
        });

        self.imp().message_view_box.append(&web_view);
        *self.imp().message_view.borrow_mut() = Some(web_view);
        self.show_message_placeholder("Select a conversation");
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
                    .insert(user_id.clone(), display_name);
                self.imp().pending_user_ids.borrow_mut().remove(&user_id);
                self.render_conversations();
                self.rerender_current_messages();
            }
            RuntimeEvent::ImageAssetLoaded { key, data_uri } => {
                crate::debug::log(
                    "ui",
                    &format!("ImageAssetLoaded key={}", crate::debug::url_for_log(&key)),
                );
                let imp = self.imp();
                imp.pending_image_assets.borrow_mut().remove(&key);
                imp.failed_image_assets.borrow_mut().remove(&key);
                imp.image_assets.borrow_mut().insert(key, data_uri);
                self.rerender_current_messages();
            }
            RuntimeEvent::ImageAssetFailed { key } => {
                crate::debug::log(
                    "ui",
                    &format!("ImageAssetFailed key={}", crate::debug::url_for_log(&key)),
                );
                let imp = self.imp();
                imp.pending_image_assets.borrow_mut().remove(&key);
                imp.failed_image_assets.borrow_mut().insert(key);
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
                if let Some(channel_id) = self.selected_channel_id() {
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
        self.send_command(RuntimeCommand::StartOAuth {
            client_id,
            debug_auth: self.imp().auth_debug.get(),
        });
    }

    fn search_messages(&self) {
        let query = self.imp().search_entry.text().trim().to_string();
        if query.is_empty() {
            self.set_status("Enter a search query");
            return;
        }
        self.imp().message_title.set_label("Search results");
        self.load_message_html(&message_html::placeholder_document(
            "Search results",
            "Searching",
        ));
        self.send_command(RuntimeCommand::SearchMessages { query });
    }

    fn post_current_message(&self) {
        let imp = self.imp();
        let Some(channel_id) = self.selected_channel_id() else {
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
        let Some(channel_id) = self.selected_channel_id() else {
            self.set_status("Select a conversation");
            return;
        };
        let Some(thread_ts) = self.selected_thread_ts() else {
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
        let Some(channel_id) = self.selected_channel_id() else {
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

    fn handle_message_view_uri(&self, uri: &str) -> bool {
        let Ok(url) = url::Url::parse(uri) else {
            return false;
        };

        match url.scheme() {
            "conduit" => self.handle_message_action_url(&url),
            "http" | "https" => {
                self.open_external_link(uri);
                true
            }
            "about" | "app" => false,
            _ => {
                self.set_status("Unsupported message link");
                true
            }
        }
    }

    fn handle_message_action_url(&self, url: &url::Url) -> bool {
        match url.host_str() {
            Some("thread") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                self.set_status("Loading thread");
                self.send_command(RuntimeCommand::LoadThread { channel_id, ts });
                true
            }
            Some("reaction") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                let name = query_param(url, "name").unwrap_or_else(|| "thumbsup".to_string());
                let add = query_param(url, "add").is_none_or(|value| value == "true");
                let thread_ts = query_param(url, "thread_ts");
                self.send_command(RuntimeCommand::SetReaction {
                    channel_id,
                    ts,
                    name,
                    add,
                    thread_ts,
                });
                self.set_status(if add {
                    "Adding reaction"
                } else {
                    "Removing reaction"
                });
                true
            }
            _ => true,
        }
    }

    fn open_external_link(&self, uri: &str) {
        if let Err(error) = open::that(uri) {
            self.set_status(&format!("Failed to open link: {error}"));
        }
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

    pub(crate) fn set_auth_debug(&self, enabled: bool) {
        if enabled {
            eprintln!("[conduit::auth] Slack OAuth debug logging enabled");
        }
        self.imp().auth_debug.set(enabled);
    }

    fn reset_workspace_state(&self) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = None;
        *imp.selected_thread_ts.borrow_mut() = None;
        *imp.current_user_id.borrow_mut() = None;
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.user_names.borrow_mut().clear();
        imp.pending_user_ids.borrow_mut().clear();
        imp.image_assets.borrow_mut().clear();
        imp.pending_image_assets.borrow_mut().clear();
        imp.failed_image_assets.borrow_mut().clear();
        imp.current_channel_messages.borrow_mut().clear();
        imp.current_thread_messages.borrow_mut().clear();
        imp.current_search_results.borrow_mut().clear();
        imp.current_saved_items.borrow_mut().clear();
        imp.current_main_view.set(MainMessageView::Placeholder);
        self.clear_list(&imp.conversation_list);
        self.clear_list(&imp.thread_list);
        self.show_message_placeholder("Select a conversation");
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
        *self.imp().conversations.borrow_mut() = conversations;
        self.request_conversation_user_names();
        self.render_conversations();
    }

    fn render_conversations(&self) {
        let imp = self.imp();
        let mut conversations = imp.conversations.borrow().clone();
        let user_names = imp.user_names.borrow().clone();
        conversations.sort_by_key(|conversation| {
            conversation
                .display_name_with_users(&user_names)
                .to_lowercase()
        });

        self.clear_list(&imp.conversation_list);

        if conversations.is_empty() {
            self.append_placeholder(&imp.conversation_list, "No conversations");
            return;
        }

        for conversation in conversations {
            let row = gtk::ListBoxRow::new();
            let title = conversation.display_name_with_users(&user_names);
            let button = gtk::Button::with_label(&title);
            button.set_halign(gtk::Align::Fill);
            button.set_hexpand(true);
            button.add_css_class("flat");

            let channel_id = conversation.id.clone();
            let weak_window = self.downgrade();
            button.connect_clicked(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.select_conversation(&channel_id, &title);
                }
            });

            row.set_child(Some(&button));
            imp.conversation_list.append(&row);
        }

        if imp.current_main_view.get() == MainMessageView::Conversation {
            if let Some(channel_id) = self.selected_channel_id() {
                imp.message_title
                    .set_label(&self.conversation_title(&channel_id));
            }
        }
    }

    fn select_conversation(&self, channel_id: &str, title: &str) {
        crate::debug::log(
            "ui",
            &format!("select_conversation channel_id={channel_id} title={title}"),
        );
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        *imp.selected_thread_ts.borrow_mut() = None;
        imp.current_main_view.set(MainMessageView::Conversation);
        imp.message_title.set_label(title);
        imp.thread_pane.set_visible(false);
        self.show_message_placeholder("Loading messages");
        self.send_command(RuntimeCommand::LoadHistory {
            channel_id: channel_id.to_string(),
        });
    }

    fn populate_history(&self, channel_id: &str, messages: Vec<SlackMessage>) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        imp.current_main_view.set(MainMessageView::Conversation);
        self.request_image_assets(messages.iter());
        let context = self.message_html_context();
        crate::debug::log(
            "ui",
            &format!(
                "populate_history channel_id={channel_id} messages={} image_assets={} pending_images={} failed_images={}",
                messages.len(),
                context.image_assets.len(),
                imp.pending_image_assets.borrow().len(),
                context.failed_image_urls.len()
            ),
        );
        self.load_message_html(&message_html::conversation_document(
            channel_id, &messages, &context,
        ));
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

        let context = self.message_render_context();
        for message in messages {
            let row = self.message_row(channel_id, &message, true, &context);
            imp.thread_list.append(&row);
        }
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_label("Search results");
        imp.current_main_view.set(MainMessageView::Search);
        *imp.current_search_results.borrow_mut() = results.clone();
        let context = self.message_html_context();
        self.load_message_html(&message_html::search_results_document(&results, &context));
    }

    fn populate_saved_items(&self, items: Vec<SavedItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Saved items");
        imp.current_main_view.set(MainMessageView::Saved);
        *imp.current_saved_items.borrow_mut() = items.clone();
        let saved_messages = items
            .iter()
            .filter_map(|item| item.message.as_ref())
            .collect::<Vec<_>>();
        let messages_for_names = saved_messages
            .iter()
            .map(|message| (*message).clone())
            .collect::<Vec<_>>();
        self.request_user_names(&messages_for_names);
        self.request_image_assets(saved_messages);
        let context = self.message_html_context();
        self.load_message_html(&message_html::saved_items_document(&items, &context));
    }

    fn message_row(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        in_thread: bool,
        context: &MessageRenderContext,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
        container.set_margin_top(10);
        container.set_margin_bottom(10);
        container.set_margin_start(10);
        container.set_margin_end(10);

        let heading = gtk::Label::new(Some(&self.message_author_label(message, context)));
        heading.set_xalign(0.0);
        heading.add_css_class("caption");
        container.append(&heading);

        rendering::append_message_content(&container, message, &context.user_names);

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
        let reacted = message.user_reacted("thumbsup", context.current_user_id.as_deref());
        let reaction_button = gtk::Button::with_label(if reacted { "Remove +1" } else { "+1" });
        reaction_button.set_halign(gtk::Align::Start);
        let reaction_channel_id = channel_id.to_string();
        let reaction_ts = message.ts.clone();
        let reaction_thread_ts = if in_thread {
            context.selected_thread_ts.clone()
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
        let user_names = self.imp().user_names.borrow().clone();
        self.imp()
            .conversations
            .borrow()
            .iter()
            .find(|conversation| conversation.id == channel_id)
            .map(|conversation| conversation.display_name_with_users(&user_names))
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

    fn show_message_placeholder(&self, text: &str) {
        self.imp()
            .current_main_view
            .set(MainMessageView::Placeholder);
        self.load_message_html(&message_html::placeholder_document("Messages", text));
    }

    fn load_message_html(&self, html: &str) {
        if let Some(web_view) = self.imp().message_view.borrow().as_ref() {
            crate::debug::log("ui", &format!("load_message_html bytes={}", html.len()));
            web_view.load_html(html, Some(message_html::base_uri()));
        }
    }

    fn send_command(&self, command: RuntimeCommand) {
        let runtime = self.imp().runtime.borrow().clone();
        if let Some(runtime) = runtime {
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

        self.request_user_ids(ids);
    }

    fn request_conversation_user_names(&self) {
        let mut ids = self
            .imp()
            .conversations
            .borrow()
            .iter()
            .filter(|conversation| conversation.is_im.unwrap_or(false))
            .filter_map(|conversation| conversation.user.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();

        self.request_user_ids(ids);
    }

    fn request_user_ids(&self, ids: Vec<String>) {
        let known_users = self.imp().user_names.borrow().clone();
        let mut pending_user_ids = self.imp().pending_user_ids.borrow_mut();
        let missing_ids = ids
            .into_iter()
            .filter(|user_id| {
                !known_users.contains_key(user_id) && pending_user_ids.insert(user_id.clone())
            })
            .collect::<Vec<_>>();
        drop(pending_user_ids);

        for user_id in missing_ids {
            self.send_command(RuntimeCommand::LoadUser { user_id });
        }
    }

    fn request_image_assets<'a>(&self, messages: impl IntoIterator<Item = &'a SlackMessage>) {
        let mut requests = messages
            .into_iter()
            .flat_map(|message| message.files.as_ref().into_iter().flatten())
            .filter_map(image_asset_request)
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.0.cmp(&right.0));
        requests.dedup_by(|left, right| left.0 == right.0);

        let known_assets = self.imp().image_assets.borrow().clone();
        let failed_assets = self.imp().failed_image_assets.borrow().clone();
        let mut pending_assets = self.imp().pending_image_assets.borrow_mut();
        let missing_requests = requests
            .into_iter()
            .filter(|(key, _)| {
                !known_assets.contains_key(key)
                    && !failed_assets.contains(key)
                    && pending_assets.insert(key.clone())
            })
            .collect::<Vec<_>>();
        drop(pending_assets);

        crate::debug::log(
            "ui",
            &format!("request_image_assets missing={}", missing_requests.len()),
        );
        for (key, url) in missing_requests {
            crate::debug::log(
                "ui",
                &format!(
                    "request_image_asset key={} url={}",
                    crate::debug::url_for_log(&key),
                    crate::debug::url_for_log(&url)
                ),
            );
            self.send_command(RuntimeCommand::LoadImageAsset { key, url });
        }
    }

    fn message_author_label(
        &self,
        message: &SlackMessage,
        context: &MessageRenderContext,
    ) -> String {
        if let Some(user_id) = message.user.as_ref() {
            if let Some(name) = context.user_names.get(user_id) {
                return name.clone();
            }
        }

        message.author_label()
    }

    fn rerender_current_messages(&self) {
        let snapshot = self.current_message_snapshot();

        match snapshot.main_view {
            MainMessageView::Conversation => {
                if let Some(channel_id) = snapshot.channel_id.as_deref() {
                    if !snapshot.channel_messages.is_empty() {
                        self.populate_history(channel_id, snapshot.channel_messages);
                    }
                }
            }
            MainMessageView::Search => self.populate_search_results(snapshot.search_results),
            MainMessageView::Saved => self.populate_saved_items(snapshot.saved_items),
            MainMessageView::Placeholder => {}
        }

        if let Some(channel_id) = snapshot.channel_id {
            if let Some(thread_ts) = snapshot.thread_ts {
                if !snapshot.thread_messages.is_empty() {
                    self.populate_thread(&channel_id, &thread_ts, snapshot.thread_messages);
                }
            }
        }
    }

    fn message_render_context(&self) -> MessageRenderContext {
        let imp = self.imp();
        MessageRenderContext {
            user_names: imp.user_names.borrow().clone(),
            current_user_id: imp.current_user_id.borrow().clone(),
            selected_thread_ts: imp.selected_thread_ts.borrow().clone(),
        }
    }

    fn message_html_context(&self) -> MessageHtmlContext {
        let imp = self.imp();
        MessageHtmlContext {
            user_names: imp.user_names.borrow().clone(),
            current_user_id: imp.current_user_id.borrow().clone(),
            image_assets: imp.image_assets.borrow().clone(),
            failed_image_urls: imp.failed_image_assets.borrow().clone(),
        }
    }

    fn current_message_snapshot(&self) -> CurrentMessageSnapshot {
        let imp = self.imp();
        CurrentMessageSnapshot {
            channel_id: imp.selected_channel.borrow().clone(),
            thread_ts: imp.selected_thread_ts.borrow().clone(),
            channel_messages: imp.current_channel_messages.borrow().clone(),
            thread_messages: imp.current_thread_messages.borrow().clone(),
            search_results: imp.current_search_results.borrow().clone(),
            saved_items: imp.current_saved_items.borrow().clone(),
            main_view: imp.current_main_view.get(),
        }
    }

    fn selected_channel_id(&self) -> Option<String> {
        self.imp().selected_channel.borrow().clone()
    }

    fn selected_thread_ts(&self) -> Option<String> {
        self.imp().selected_thread_ts.borrow().clone()
    }
}
