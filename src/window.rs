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
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::activity::{self, ActivityItem};
use crate::auth;
use crate::config;
use crate::message_html::{self, MessageHtmlContext, TimelineScrollBehavior};
use crate::models::{SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage};
use crate::rendering;
use crate::runtime::{AppRuntime, RuntimeCommand, RuntimeEvent};
use crate::sidebar::{self, SidebarRowModel, SidebarSectionKind, SidebarSectionModel};

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
        pub auth_intro_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub client_id_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub browser_session_check: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub xoxc_token_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub xoxd_token_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub user_agent_entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub setup_hint_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub connect_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub connection_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub workspace_title_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub home_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub activity_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub files_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub saved_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub refresh_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub sidebar_filter_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub sidebar_unread_filter_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub sidebar_all_filter_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub conversation_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub workspace_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_view_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub message_thread_paned: TemplateChild<gtk::Paned>,
        #[template_child]
        pub message_entry: TemplateChild<gtk::TextView>,
        #[template_child]
        pub send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upload_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upload_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child]
        pub message_search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub message_search_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub thread_pane: TemplateChild<gtk::Box>,
        #[template_child]
        pub thread_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub thread_view_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub thread_entry: TemplateChild<gtk::TextView>,
        #[template_child]
        pub thread_send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub close_thread_button: TemplateChild<gtk::Button>,

        pub runtime: RefCell<Option<AppRuntime>>,
        pub events: RefCell<Option<Receiver<RuntimeEvent>>>,
        pub connect_requested: Cell<bool>,
        pub auth_debug: Cell<bool>,
        pub conversations: RefCell<Vec<SlackConversation>>,
        pub(super) sidebar_row_actions: RefCell<HashMap<i32, SidebarRowAction>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub workspace_name: RefCell<Option<String>>,
        pub workspace_url: RefCell<Option<String>>,
        pub sidebar_loading: Cell<bool>,
        pub sidebar_error: RefCell<Option<String>>,
        pub current_channel_messages: RefCell<Vec<SlackMessage>>,
        pub channel_message_cache: RefCell<HashMap<String, Vec<SlackMessage>>>,
        pub current_thread_messages: RefCell<Vec<SlackMessage>>,
        pub current_search_results: RefCell<Vec<SearchMatch>>,
        pub current_files: RefCell<Vec<SlackFile>>,
        pub current_saved_items: RefCell<Vec<SavedItem>>,
        pub current_main_view: Cell<MainMessageView>,
        pub current_user_id: RefCell<Option<String>>,
        pub selected_channel: RefCell<Option<String>>,
        pub selected_thread_ts: RefCell<Option<String>>,
        pub channel_history_cursors: RefCell<HashMap<String, String>>,
        pub loading_channel_histories: RefCell<HashSet<String>>,
        pub force_bottom_channel_renders: RefCell<HashSet<String>>,
        pub thread_history_cursors: RefCell<HashMap<String, String>>,
        pub message_view: RefCell<Option<webkit6::WebView>>,
        pub thread_view: RefCell<Option<webkit6::WebView>>,
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
struct CurrentMessageSnapshot {
    channel_id: Option<String>,
    thread_ts: Option<String>,
    channel_messages: Vec<SlackMessage>,
    thread_messages: Vec<SlackMessage>,
    search_results: Vec<SearchMatch>,
    files: Vec<SlackFile>,
    saved_items: Vec<SavedItem>,
    main_view: MainMessageView,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarRowAction {
    channel_id: String,
    title: String,
}

impl SidebarRowAction {
    fn from_model(model: &SidebarRowModel) -> Self {
        Self {
            channel_id: model.id.clone(),
            title: model.title.clone(),
        }
    }
}

fn sidebar_row_action_for_index(
    actions: &HashMap<i32, SidebarRowAction>,
    row_index: i32,
) -> Option<SidebarRowAction> {
    actions.get(&row_index).cloned()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MainMessageView {
    #[default]
    Placeholder,
    Conversation,
    Activity,
    Search,
    Files,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationHistorySelectionAction {
    RenderCurrent,
    RenderCached,
    RenderCachedAndRefresh,
    RequestFresh,
    AwaitFresh,
}

fn sidebar_selected_channel(
    main_view: MainMessageView,
    selected_channel: Option<String>,
) -> Option<String> {
    (main_view == MainMessageView::Conversation)
        .then_some(selected_channel)
        .flatten()
}

fn sidebar_conversation_matches_filters(
    conversation: &SlackConversation,
    user_names: &HashMap<String, String>,
    query: &str,
    unread_only: bool,
) -> bool {
    let matches_query = query.is_empty()
        || conversation
            .display_name_with_users(user_names)
            .to_lowercase()
            .contains(query)
        || conversation.id.to_lowercase().contains(query);
    let matches_unread = !unread_only || conversation.has_unread_activity();

    matches_query && matches_unread
}

fn sidebar_loading_change_needs_render(
    has_conversations: bool,
    current_loading: bool,
    next_loading: bool,
) -> bool {
    current_loading != next_loading && !has_conversations
}

fn sidebar_error_change_needs_render(has_conversations: bool) -> bool {
    !has_conversations
}

fn sidebar_user_name_update_needs_render(
    conversations: &[SlackConversation],
    user_id: &str,
    sidebar_loading: bool,
) -> bool {
    !sidebar_loading
        && conversations.iter().any(|conversation| {
            conversation.is_im.unwrap_or(false) && conversation.user.as_deref() == Some(user_id)
        })
}

fn conversation_switcher_items(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    query: &str,
) -> Vec<SidebarRowModel> {
    let query = query.trim().to_lowercase();
    let mut items = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .map(|conversation| {
            let kind = sidebar::conversation_kind(conversation);
            SidebarRowModel {
                id: conversation.id.clone(),
                title: conversation.display_name_with_users(user_names),
                kind,
                unread_count: conversation.unread_activity_count(),
                selected: false,
                private: conversation.is_private.unwrap_or(false)
                    || conversation.is_group.unwrap_or(false)
                    || matches!(kind, sidebar::ConversationKind::PrivateChannel),
                muted: conversation.is_muted_conversation(),
                external: conversation.is_external_conversation(),
            }
        })
        .filter(|item| {
            query.is_empty()
                || item.title.to_lowercase().contains(&query)
                || item.id.to_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();

    items.sort_by_key(|item| (switcher_title_sort_key(&item.title), item.id.to_lowercase()));
    items
}

fn switcher_title_sort_key(title: &str) -> String {
    title.trim_start_matches('#').trim_start().to_lowercase()
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

fn create_cache_directory(path: &Path) {
    if let Err(error) = std::fs::create_dir_all(path) {
        crate::debug::log(
            "ui",
            &format!(
                "failed to create cache directory {}: {error}",
                path.display()
            ),
        );
    }
}

fn message_permalink(workspace_url: &str, channel_id: &str, ts: &str) -> Option<String> {
    let ts = slack_permalink_ts(ts)?;
    Some(format!(
        "{}/archives/{}/p{}",
        workspace_url.trim_end_matches('/'),
        channel_id,
        ts
    ))
}

fn slack_permalink_ts(ts: &str) -> Option<String> {
    let (seconds, fraction) = ts.split_once('.')?;
    if seconds.is_empty() || !seconds.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }

    let mut fraction = fraction
        .chars()
        .take(6)
        .filter(|character| character.is_ascii_digit())
        .collect::<String>();
    while fraction.len() < 6 {
        fraction.push('0');
    }

    Some(format!("{seconds}{fraction}"))
}

fn thread_history_key(channel_id: &str, ts: &str) -> String {
    format!("{channel_id}:{ts}")
}

fn merge_message_pages(existing: &[SlackMessage], page: &[SlackMessage]) -> Vec<SlackMessage> {
    let mut messages = existing.to_vec();
    messages.extend(page.iter().cloned());
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages
}

fn conversation_history_selection_action(
    requested_channel: &str,
    selected_channel: Option<&str>,
    current_messages: &[SlackMessage],
    cached_messages: Option<&[SlackMessage]>,
    fresh_load_in_progress: bool,
) -> ConversationHistorySelectionAction {
    if selected_channel == Some(requested_channel) && !current_messages.is_empty() {
        ConversationHistorySelectionAction::RenderCurrent
    } else if cached_messages.is_some_and(|messages| !messages.is_empty()) && fresh_load_in_progress
    {
        ConversationHistorySelectionAction::RenderCached
    } else if cached_messages.is_some_and(|messages| !messages.is_empty()) {
        ConversationHistorySelectionAction::RenderCachedAndRefresh
    } else if fresh_load_in_progress {
        ConversationHistorySelectionAction::AwaitFresh
    } else {
        ConversationHistorySelectionAction::RequestFresh
    }
}

fn history_event_updates_fresh_metadata(cached: bool) -> bool {
    !cached
}

fn history_event_marks_read(cached: bool, append_older: bool) -> bool {
    !cached && !append_older
}

fn channel_history_scroll_behavior(
    append_older: bool,
    force_bottom: bool,
) -> TimelineScrollBehavior {
    if append_older {
        TimelineScrollBehavior::PreservePrepend
    } else if force_bottom {
        TimelineScrollBehavior::Bottom
    } else {
        TimelineScrollBehavior::StickToBottom
    }
}

fn text_view_text(text_view: &gtk::TextView) -> String {
    let buffer = text_view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

fn set_text_view_text(text_view: &gtk::TextView, text: &str) {
    text_view.buffer().set_text(text);
}

fn browser_session_input(
    xoxc_token: &str,
    xoxd_token: &str,
) -> std::result::Result<(String, String), &'static str> {
    let xoxc_token = xoxc_token.trim();
    let xoxd_token = xoxd_token.trim();

    if xoxc_token.is_empty() || xoxd_token.is_empty() {
        return Err("Enter XOXC and XOXD tokens");
    }

    Ok((xoxc_token.to_string(), xoxd_token.to_string()))
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
        let network_session = self.create_message_network_session();

        let message_view = self.create_message_web_view(&network_session);
        self.imp().message_view_box.append(&message_view);
        *self.imp().message_view.borrow_mut() = Some(message_view);

        let thread_view = self.create_message_web_view(&network_session);
        self.imp().thread_view_box.append(&thread_view);
        *self.imp().thread_view.borrow_mut() = Some(thread_view);

        self.show_message_placeholder("Select a conversation");
        self.load_thread_html(&message_html::placeholder_document(
            "Thread",
            "No thread open",
        ));
    }

    fn create_message_network_session(&self) -> webkit6::NetworkSession {
        let data_dir = config::webkit_data_dir();
        let cache_dir = config::webkit_cache_dir();
        create_cache_directory(&data_dir);
        create_cache_directory(&cache_dir);

        let data_dir = data_dir.to_string_lossy().into_owned();
        let cache_dir = cache_dir.to_string_lossy().into_owned();
        webkit6::NetworkSession::new(Some(&data_dir), Some(&cache_dir))
    }

    fn create_message_web_view(
        &self,
        network_session: &webkit6::NetworkSession,
    ) -> webkit6::WebView {
        let settings = webkit6::Settings::new();
        settings.set_allow_file_access_from_file_urls(false);
        settings.set_allow_universal_access_from_file_urls(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_javascript(false);
        settings.set_enable_media(false);
        settings.set_enable_webgl(false);
        settings.set_enable_webaudio(false);

        let web_view = webkit6::WebView::builder()
            .network_session(network_session)
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

        web_view
    }

    fn setup_callbacks(&self) {
        let imp = self.imp();

        self.setup_window_actions();
        self.connect_widget(&imp.connect_button.get(), |window| window.start_auth());
        self.connect_widget(&imp.home_button.get(), |window| window.show_home());
        self.connect_widget(&imp.activity_button.get(), |window| window.show_activity());
        self.connect_widget(&imp.files_button.get(), |window| window.show_files());
        self.connect_widget(&imp.refresh_button.get(), |window| {
            window.refresh_conversations()
        });
        self.connect_widget(&imp.saved_button.get(), |window| window.show_later());
        self.connect_widget(&imp.message_search_button.get(), |window| {
            window.search_messages()
        });
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
        imp.browser_session_check.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.update_auth_mode_ui();
            }
        });

        let weak_window = self.downgrade();
        imp.sidebar_filter_entry.connect_search_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.render_conversations();
            }
        });

        let weak_window = self.downgrade();
        imp.sidebar_unread_filter_button.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.render_conversations();
            }
        });

        let weak_window = self.downgrade();
        imp.sidebar_all_filter_button.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.render_conversations();
            }
        });

        let weak_window = self.downgrade();
        imp.conversation_list.connect_row_activated(move |_, row| {
            if let Some(window) = weak_window.upgrade() {
                window.activate_sidebar_row(row.index());
            }
        });

        self.connect_text_view_send_shortcut(&imp.message_entry.get(), |window| {
            window.post_current_message()
        });
        self.connect_text_view_send_shortcut(&imp.thread_entry.get(), |window| {
            window.post_thread_reply()
        });

        let weak_window = self.downgrade();
        imp.message_search_entry.connect_activate(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.search_messages();
            }
        });
    }

    fn setup_window_actions(&self) {
        let sign_out_action = gio::SimpleAction::new("sign-out", None);
        let weak_window = self.downgrade();
        sign_out_action.connect_activate(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.send_command(RuntimeCommand::SignOut);
            }
        });
        self.add_action(&sign_out_action);

        let switch_action = gio::SimpleAction::new("switch-conversation", None);
        let weak_window = self.downgrade();
        switch_action.connect_activate(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.show_conversation_switcher();
            }
        });
        self.add_action(&switch_action);

        if let Some(application) = self.application() {
            application.set_accels_for_action("win.switch-conversation", &["<control>k"]);
        }
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

    fn connect_text_view_send_shortcut<F>(&self, text_view: &gtk::TextView, callback: F)
    where
        F: Fn(&Self) + 'static,
    {
        let controller = gtk::EventControllerKey::new();
        let weak_window = self.downgrade();
        controller.connect_key_pressed(move |_, key, _, state| {
            if key == gtk::gdk::Key::Return && state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                if let Some(window) = weak_window.upgrade() {
                    callback(&window);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        text_view.add_controller(controller);
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
                    if status == "Loading conversations" {
                        self.set_sidebar_loading(true);
                    }
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
            RuntimeEvent::ConversationsLoadFailed(error) => {
                if !self.imp().connect_requested.get() {
                    self.show_conversation_load_error(&error);
                }
            }
            RuntimeEvent::HistoryLoaded {
                channel_id,
                messages,
                has_more,
                next_cursor,
                append_older,
                cached,
            } => {
                if history_event_updates_fresh_metadata(cached) {
                    self.set_channel_history_cursor(&channel_id, has_more, next_cursor);
                }
                if history_event_marks_read(cached, append_older) {
                    self.imp()
                        .loading_channel_histories
                        .borrow_mut()
                        .remove(&channel_id);
                    self.notify_if_new_messages(&channel_id, &messages);
                    self.mark_conversation_locally_read(&channel_id);
                }
                let rendered_messages = if append_older {
                    merge_message_pages(&self.imp().current_channel_messages.borrow(), &messages)
                } else {
                    messages
                };
                let scroll_behavior =
                    self.next_channel_history_scroll_behavior(&channel_id, append_older);
                self.request_user_names(&rendered_messages);
                self.populate_history_with_scroll(&channel_id, rendered_messages, scroll_behavior);
            }
            RuntimeEvent::ThreadLoaded {
                channel_id,
                ts,
                messages,
                has_more,
                next_cursor,
                append_older,
            } => {
                self.set_thread_history_cursor(&channel_id, &ts, has_more, next_cursor);
                let rendered_messages = if append_older {
                    merge_message_pages(&self.imp().current_thread_messages.borrow(), &messages)
                } else {
                    messages
                };
                self.request_user_names(&rendered_messages);
                *self.imp().current_thread_messages.borrow_mut() = rendered_messages.clone();
                self.populate_thread(&channel_id, &ts, rendered_messages);
            }
            RuntimeEvent::SearchLoaded(results) => self.populate_search_results(results),
            RuntimeEvent::FilesLoaded(files) => self.populate_files(files),
            RuntimeEvent::SavedItemsLoaded(items) => self.populate_saved_items(items),
            RuntimeEvent::UserLoaded {
                user_id,
                display_name,
            } => {
                let should_render_sidebar = {
                    let imp = self.imp();
                    let conversations = imp.conversations.borrow();
                    sidebar_user_name_update_needs_render(
                        &conversations,
                        &user_id,
                        imp.sidebar_loading.get(),
                    )
                };
                self.imp()
                    .user_names
                    .borrow_mut()
                    .insert(user_id.clone(), display_name);
                self.imp().pending_user_ids.borrow_mut().remove(&user_id);
                if should_render_sidebar {
                    self.render_conversations();
                }
                self.refresh_current_conversation_title();
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
                set_text_view_text(&self.imp().message_entry, "");
                self.imp().send_button.set_sensitive(true);
                self.imp().thread_send_button.set_sensitive(true);
                self.set_status("Message sent");
                let thread_ts = message.thread_ts.clone();
                if thread_ts.is_none() {
                    self.force_next_channel_bottom_render(&channel_id);
                }
                self.reload_after_message(&channel_id, thread_ts.as_deref());
            }
            RuntimeEvent::ReactionUpdated {
                channel_id,
                thread_ts,
            } => {
                self.set_status("Reaction updated");
                self.reload_after_message(&channel_id, thread_ts.as_deref());
            }
            RuntimeEvent::SavedUpdated {
                channel_id,
                saved,
                thread_ts,
            } => {
                self.set_status(if saved {
                    "Saved for later"
                } else {
                    "Removed from saved items"
                });
                if self.imp().current_main_view.get() == MainMessageView::Saved {
                    self.send_command(RuntimeCommand::LoadSavedItems);
                } else {
                    self.reload_after_message(&channel_id, thread_ts.as_deref());
                }
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
                set_text_view_text(&imp.message_entry, "");
                self.set_status(&format!("Uploaded {name}"));
                if let Some(channel_id) = self.selected_channel_id() {
                    self.force_next_channel_bottom_render(&channel_id);
                    self.send_command(RuntimeCommand::LoadHistory { channel_id });
                }
            }
        }
    }

    fn configure_auth_ui(&self) {
        let imp = self.imp();
        if let Some(client_id) = config::slack_client_id() {
            imp.client_id_entry.set_text(&client_id);
        } else {
            imp.setup_hint_label.set_label(&format!(
                "Use redirect URL {} in the Slack app settings.",
                auth::OAuthConfig::new("").redirect_uri()
            ));
        }
        self.update_auth_mode_ui();
    }

    fn update_auth_mode_ui(&self) {
        let imp = self.imp();
        let browser_session = imp.browser_session_check.is_active();
        let has_packaged_client_id = config::slack_client_id().is_some();

        imp.client_id_entry
            .set_visible(!browser_session && !has_packaged_client_id);
        imp.xoxc_token_entry.set_visible(browser_session);
        imp.xoxd_token_entry.set_visible(browser_session);
        imp.user_agent_entry.set_visible(browser_session);

        if browser_session {
            imp.auth_intro_label.set_label(
                "Paste browser-session credentials. They will be stored in the system keyring.",
            );
            imp.setup_hint_label.set_visible(true);
            imp.setup_hint_label
                .set_label("Paste your Slack browser xoxc token and xoxd cookie.");
            imp.connect_button.set_label("Import Browser Session");
        } else {
            imp.auth_intro_label.set_label(
                "Approve Conduit in your browser. Your Slack token will be stored in the system keyring.",
            );
            imp.setup_hint_label.set_visible(!has_packaged_client_id);
            imp.setup_hint_label.set_label(&format!(
                "Use redirect URL {} in the Slack app settings.",
                auth::OAuthConfig::new("").redirect_uri()
            ));
            imp.connect_button.set_label("Connect Workspace");
        }
    }

    fn start_auth(&self) {
        if self.imp().browser_session_check.is_active() {
            self.start_browser_session();
        } else {
            self.start_oauth();
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

    fn start_browser_session(&self) {
        let imp = self.imp();
        let (xoxc_token, xoxd_token) =
            match browser_session_input(&imp.xoxc_token_entry.text(), &imp.xoxd_token_entry.text())
            {
                Ok(tokens) => tokens,
                Err(status) => {
                    self.show_login(status);
                    return;
                }
            };
        let user_agent = imp.user_agent_entry.text().trim().to_string();
        let user_agent = (!user_agent.is_empty()).then_some(user_agent);

        self.imp().connect_requested.set(false);
        imp.connect_button.set_sensitive(false);
        self.show_loading("Validating Slack browser session");
        self.send_command(RuntimeCommand::StartBrowserSession {
            xoxc_token,
            xoxd_token,
            user_agent,
        });
    }

    fn refresh_conversations(&self) {
        self.set_sidebar_loading(true);
        self.send_command(RuntimeCommand::RefreshConversations);
    }

    fn show_home(&self) {
        if let Some(channel_id) = self.selected_channel_id() {
            let title = self.conversation_title(&channel_id);
            self.select_conversation(&channel_id, &title);
        } else {
            self.imp()
                .current_main_view
                .set(MainMessageView::Placeholder);
            self.imp().message_title.set_label("Select a conversation");
            self.show_message_placeholder("Select a conversation");
            self.close_thread();
            self.render_conversations();
        }
    }

    fn show_activity(&self) {
        self.close_thread();
        let items = self.activity_items();
        self.populate_activity(items);
    }

    fn show_files(&self) {
        self.close_thread();
        self.imp().current_main_view.set(MainMessageView::Files);
        self.imp().message_title.set_label("Files");
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(
            "Files",
            "Loading files",
        ));
        self.send_command(RuntimeCommand::LoadFiles);
    }

    fn show_later(&self) {
        self.imp().current_main_view.set(MainMessageView::Saved);
        self.imp().message_title.set_label("Later");
        self.close_thread();
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(
            "Later",
            "Loading saved items",
        ));
        self.send_command(RuntimeCommand::LoadSavedItems);
    }

    fn search_messages(&self) {
        let query = self.imp().message_search_entry.text().trim().to_string();
        if query.is_empty() {
            self.set_status("Enter a message search query");
            return;
        }
        self.close_thread();
        self.imp().current_main_view.set(MainMessageView::Search);
        self.render_conversations();
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
        let text = text_view_text(&imp.message_entry).trim().to_string();
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
        let text = text_view_text(&imp.thread_entry).trim().to_string();
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
        let initial_comment = text_view_text(&self.imp().message_entry).trim().to_string();

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
        imp.current_thread_messages.borrow_mut().clear();
        set_text_view_text(&imp.thread_entry, "");
        imp.thread_pane.set_visible(false);
        self.load_thread_html(&message_html::placeholder_document(
            "Thread",
            "No thread open",
        ));
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
            Some("load-older") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(cursor) = query_param(url, "cursor") else {
                    return true;
                };
                if let Some(ts) = query_param(url, "thread_ts") {
                    self.set_status("Loading more replies");
                    self.send_command(RuntimeCommand::LoadOlderThread {
                        channel_id,
                        ts,
                        cursor,
                    });
                } else {
                    self.set_status("Loading older messages");
                    self.send_command(RuntimeCommand::LoadOlderHistory { channel_id, cursor });
                }
                true
            }
            Some("activity-open") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let title = self.conversation_title(&channel_id);
                self.select_conversation(&channel_id, &title);
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
            Some("save") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                let add = query_param(url, "add").is_none_or(|value| value == "true");
                let thread_ts = query_param(url, "thread_ts");
                self.send_command(RuntimeCommand::SetSaved {
                    channel_id,
                    ts,
                    add,
                    thread_ts,
                });
                self.set_status(if add {
                    "Saving message"
                } else {
                    "Removing saved message"
                });
                true
            }
            Some("copy-message") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                self.copy_message_text(&channel_id, &ts);
                true
            }
            Some("copy-link") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                self.copy_message_link(&channel_id, &ts);
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

    fn copy_message_text(&self, channel_id: &str, ts: &str) {
        let Some(message) = self.find_message(channel_id, ts) else {
            self.set_status("Message is no longer loaded");
            return;
        };

        let text = message.body_text();
        if text.trim().is_empty() {
            self.set_status("Message has no text to copy");
            return;
        }

        self.copy_to_clipboard(&text, "Copied message");
    }

    fn copy_message_link(&self, channel_id: &str, ts: &str) {
        let Some(workspace_url) = self.imp().workspace_url.borrow().clone() else {
            self.set_status("Workspace URL is not available");
            return;
        };
        let Some(permalink) = message_permalink(&workspace_url, channel_id, ts) else {
            self.set_status("Could not build message link");
            return;
        };

        self.copy_to_clipboard(&permalink, "Copied message link");
    }

    fn copy_to_clipboard(&self, text: &str, status: &str) {
        let Some(display) = gtk::gdk::Display::default() else {
            self.set_status("Clipboard is not available");
            return;
        };

        display.clipboard().set_text(text);
        self.set_status(status);
    }

    fn find_message(&self, channel_id: &str, ts: &str) -> Option<SlackMessage> {
        let imp = self.imp();
        if imp.selected_channel.borrow().as_deref() == Some(channel_id) {
            if let Some(message) = imp
                .current_channel_messages
                .borrow()
                .iter()
                .find(|message| message.ts == ts)
                .cloned()
            {
                return Some(message);
            }

            if let Some(message) = imp
                .current_thread_messages
                .borrow()
                .iter()
                .find(|message| message.ts == ts)
                .cloned()
            {
                return Some(message);
            }
        }

        imp.current_saved_items
            .borrow()
            .iter()
            .filter(|item| item.channel.as_deref() == Some(channel_id))
            .filter_map(|item| item.message.as_ref())
            .find(|message| message.ts == ts)
            .cloned()
    }

    fn reload_after_message(&self, channel_id: &str, thread_ts: Option<&str>) {
        if let Some(thread_ts) = thread_ts {
            set_text_view_text(&self.imp().thread_entry, "");
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
        imp.channel_history_cursors.borrow_mut().clear();
        imp.loading_channel_histories.borrow_mut().clear();
        imp.force_bottom_channel_renders.borrow_mut().clear();
        imp.thread_history_cursors.borrow_mut().clear();
        *imp.current_user_id.borrow_mut() = None;
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.conversations.borrow_mut().clear();
        imp.sidebar_row_actions.borrow_mut().clear();
        imp.user_names.borrow_mut().clear();
        imp.pending_user_ids.borrow_mut().clear();
        *imp.workspace_name.borrow_mut() = None;
        *imp.workspace_url.borrow_mut() = None;
        imp.sidebar_loading.set(false);
        *imp.sidebar_error.borrow_mut() = None;
        imp.image_assets.borrow_mut().clear();
        imp.pending_image_assets.borrow_mut().clear();
        imp.failed_image_assets.borrow_mut().clear();
        imp.current_channel_messages.borrow_mut().clear();
        imp.current_thread_messages.borrow_mut().clear();
        imp.current_search_results.borrow_mut().clear();
        imp.current_files.borrow_mut().clear();
        imp.current_saved_items.borrow_mut().clear();
        imp.channel_message_cache.borrow_mut().clear();
        imp.current_main_view.set(MainMessageView::Placeholder);
        set_text_view_text(&imp.message_entry, "");
        set_text_view_text(&imp.thread_entry, "");
        imp.sidebar_filter_entry.set_text("");
        imp.sidebar_unread_filter_button.set_active(false);
        imp.sidebar_all_filter_button.set_active(false);
        imp.workspace_title_label.set_label("Workspace");
        imp.workspace_status_label.set_label("");
        self.clear_list(&imp.conversation_list);
        self.show_message_placeholder("Select a conversation");
        self.load_thread_html(&message_html::placeholder_document(
            "Thread",
            "No thread open",
        ));
    }

    fn show_workspace(&self, auth: crate::models::AuthInfo) {
        *self.imp().current_user_id.borrow_mut() = auth.user_id.clone();
        *self.imp().workspace_url.borrow_mut() = auth.url.clone();
        self.imp().connect_button.set_sensitive(true);
        let workspace_name = auth
            .team
            .or(auth.team_id)
            .unwrap_or_else(|| "Slack".to_string());
        *self.imp().workspace_name.borrow_mut() = Some(workspace_name.clone());
        self.imp().workspace_title_label.set_label(&workspace_name);
        let label = format!("Connected to {workspace_name}");
        self.set_status(&label);
        self.imp().content_stack.set_visible_child_name("workspace");
        self.set_sidebar_loading(true);
    }

    fn set_status(&self, status: &str) {
        let imp = self.imp();
        imp.status_label.set_label(status);
        imp.connection_label.set_label(status);
        imp.workspace_status_label.set_label(status);
    }

    fn set_sidebar_loading(&self, loading: bool) {
        let imp = self.imp();
        let has_conversations = !imp.conversations.borrow().is_empty();
        let should_render = sidebar_loading_change_needs_render(
            has_conversations,
            imp.sidebar_loading.get(),
            loading,
        );
        imp.sidebar_loading.set(loading);
        if loading {
            *imp.sidebar_error.borrow_mut() = None;
        }
        if should_render {
            self.render_conversations();
        }
    }

    fn show_error(&self, error: &str) {
        self.imp().send_button.set_sensitive(true);
        self.imp().thread_send_button.set_sensitive(true);
        self.imp().upload_button.set_sensitive(true);
        self.imp().upload_progress.set_visible(false);
        self.imp().loading_channel_histories.borrow_mut().clear();
        if self.imp().content_stack.visible_child_name().as_deref() == Some("loading") {
            self.show_login(error);
        } else {
            if self.imp().content_stack.visible_child_name().as_deref() == Some("workspace") {
                self.set_sidebar_error(error);
            }
            self.set_status(error);
        }
    }

    fn show_conversation_load_error(&self, error: &str) {
        self.set_sidebar_error(error);
        self.set_status(error);
    }

    fn set_sidebar_error(&self, error: &str) {
        let imp = self.imp();
        let has_conversations = !imp.conversations.borrow().is_empty();
        imp.sidebar_loading.set(false);
        *imp.sidebar_error.borrow_mut() = Some(error.to_string());
        if sidebar_error_change_needs_render(has_conversations) {
            self.render_conversations();
        }
    }

    fn populate_conversations(&self, conversations: Vec<SlackConversation>) {
        self.imp().sidebar_loading.set(false);
        *self.imp().sidebar_error.borrow_mut() = None;
        *self.imp().conversations.borrow_mut() = conversations;
        self.request_conversation_user_names();
        self.render_conversations();
        if self.imp().current_main_view.get() == MainMessageView::Activity {
            self.populate_activity(self.activity_items());
        } else {
            self.refresh_current_conversation_title();
        }
    }

    fn mark_conversation_locally_read(&self, channel_id: &str) {
        let mut conversations = self.imp().conversations.borrow_mut();
        if let Some(conversation) = conversations
            .iter_mut()
            .find(|conversation| conversation.id == channel_id)
        {
            conversation.clear_unread_activity();
        }
    }

    fn set_channel_history_cursor(
        &self,
        channel_id: &str,
        has_more: bool,
        next_cursor: Option<String>,
    ) {
        let mut cursors = self.imp().channel_history_cursors.borrow_mut();
        if let Some(cursor) = next_cursor.filter(|cursor| has_more && !cursor.trim().is_empty()) {
            cursors.insert(channel_id.to_string(), cursor);
        } else {
            cursors.remove(channel_id);
        }
    }

    fn set_thread_history_cursor(
        &self,
        channel_id: &str,
        ts: &str,
        has_more: bool,
        next_cursor: Option<String>,
    ) {
        let key = thread_history_key(channel_id, ts);
        let mut cursors = self.imp().thread_history_cursors.borrow_mut();
        if let Some(cursor) = next_cursor.filter(|cursor| has_more && !cursor.trim().is_empty()) {
            cursors.insert(key, cursor);
        } else {
            cursors.remove(&key);
        }
    }

    fn channel_load_more_url(&self, channel_id: &str) -> Option<String> {
        self.imp()
            .channel_history_cursors
            .borrow()
            .get(channel_id)
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, None))
    }

    fn thread_load_more_url(&self, channel_id: &str, ts: &str) -> Option<String> {
        self.imp()
            .thread_history_cursors
            .borrow()
            .get(&thread_history_key(channel_id, ts))
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, Some(ts)))
    }

    fn render_conversations(&self) {
        let imp = self.imp();
        let conversations = imp.conversations.borrow().clone();
        let user_names = imp.user_names.borrow().clone();
        let selected_channel =
            sidebar_selected_channel(imp.current_main_view.get(), self.selected_channel_id());
        let filtered = self.filtered_sidebar_conversations(&conversations, &user_names);
        let unread_only = imp.sidebar_unread_filter_button.is_active();
        let mut sections =
            sidebar::build_sidebar_sections(&filtered, &user_names, selected_channel.as_deref());
        if unread_only {
            sections.retain(|section| section.kind != SidebarSectionKind::Unreads);
        }

        imp.sidebar_row_actions.borrow_mut().clear();
        self.clear_list(&imp.conversation_list);

        if imp.sidebar_loading.get() && conversations.is_empty() {
            self.append_placeholder(&imp.conversation_list, "Loading conversations");
            return;
        }

        if imp.sidebar_error.borrow().is_some() && conversations.is_empty() {
            self.append_placeholder(&imp.conversation_list, "Could not load conversations");
            return;
        }

        if conversations.is_empty() {
            self.append_placeholder(&imp.conversation_list, "No conversations");
            return;
        }

        if sections.is_empty() {
            self.append_placeholder(&imp.conversation_list, "No matching conversations");
            return;
        }

        for section in sections {
            self.append_sidebar_section(&imp.conversation_list, &section);
        }
    }

    fn filtered_sidebar_conversations(
        &self,
        conversations: &[SlackConversation],
        user_names: &HashMap<String, String>,
    ) -> Vec<SlackConversation> {
        let query = self.imp().sidebar_filter_entry.text().trim().to_lowercase();
        let unread_only = self.imp().sidebar_unread_filter_button.is_active();
        let show_all = self.imp().sidebar_all_filter_button.is_active();
        let selected_channel = self.selected_channel_id();

        if show_all && query.is_empty() && !unread_only {
            return conversations.to_vec();
        }

        conversations
            .iter()
            .filter(|conversation| {
                (show_all
                    || sidebar::conversation_visible_in_default_sidebar(
                        conversation,
                        selected_channel.as_deref(),
                    ))
                    && sidebar_conversation_matches_filters(
                        conversation,
                        user_names,
                        &query,
                        unread_only,
                    )
            })
            .cloned()
            .collect()
    }

    fn append_sidebar_section(&self, list: &gtk::ListBox, section: &SidebarSectionModel) {
        let header_row = gtk::ListBoxRow::new();
        header_row.set_selectable(false);
        header_row.set_activatable(false);
        header_row.set_focusable(false);

        let header_title = section.display_title();
        let header = gtk::Label::new(Some(&header_title));
        header.set_xalign(0.0);
        header.set_margin_top(12);
        header.set_margin_bottom(3);
        header.set_margin_start(9);
        header.set_margin_end(9);
        header.add_css_class("caption");
        header.add_css_class("heading");

        header_row.set_child(Some(&header));
        list.append(&header_row);

        for row in &section.rows {
            self.append_sidebar_conversation(list, row);
        }
    }

    fn append_sidebar_conversation(&self, list: &gtk::ListBox, model: &SidebarRowModel) {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(true);
        row.set_activatable(true);
        let accessible_label = model.accessible_label();
        row.set_tooltip_text(Some(&accessible_label));
        row.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(3);
        content.set_margin_bottom(3);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let icon = gtk::Image::from_icon_name(model.kind.icon_name());
        icon.set_tooltip_text(Some(model.kind.accessible_name()));
        content.append(&icon);

        let title = gtk::Label::new(Some(&model.title));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if model.unread_count > 0 {
            title.add_css_class("heading");
        }
        content.append(&title);

        if let Some(unread_label) = model.unread_badge_label() {
            let unread = gtk::Label::new(Some(&unread_label));
            unread.add_css_class("caption");
            unread.add_css_class("heading");
            content.append(&unread);
        }

        if model.muted {
            let muted = gtk::Image::from_icon_name("notifications-disabled-symbolic");
            muted.set_tooltip_text(Some("Muted"));
            content.append(&muted);
        }

        if model.external {
            let external = gtk::Image::from_icon_name("network-workgroup-symbolic");
            external.set_tooltip_text(Some("Shared externally"));
            content.append(&external);
        }

        row.set_child(Some(&content));
        list.append(&row);
        self.register_sidebar_row_action(row.index(), model);
        if model.selected && list.selected_row().is_none() {
            list.select_row(Some(&row));
        }
    }

    fn register_sidebar_row_action(&self, row_index: i32, model: &SidebarRowModel) {
        self.imp()
            .sidebar_row_actions
            .borrow_mut()
            .insert(row_index, SidebarRowAction::from_model(model));
    }

    fn activate_sidebar_row(&self, row_index: i32) {
        let action =
            sidebar_row_action_for_index(&self.imp().sidebar_row_actions.borrow(), row_index);

        if let Some(action) = action {
            self.select_conversation(&action.channel_id, &action.title);
        }
    }

    fn show_conversation_switcher(&self) {
        let imp = self.imp();
        let conversations = imp.conversations.borrow().clone();
        let user_names = imp.user_names.borrow().clone();
        let items = conversation_switcher_items(&conversations, &user_names, "");
        if items.is_empty() {
            self.set_status("No conversations loaded");
            return;
        }

        let dialog = gtk::Window::builder()
            .title("Switch conversation")
            .transient_for(self)
            .modal(true)
            .default_width(520)
            .default_height(560)
            .build();

        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.set_margin_top(12);
        container.set_margin_bottom(12);
        container.set_margin_start(12);
        container.set_margin_end(12);
        dialog.set_child(Some(&container));

        let close_controller = gtk::EventControllerKey::new();
        let dialog_for_close = dialog.clone();
        close_controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dialog_for_close.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        dialog.add_controller(close_controller);

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some("Search conversations"));
        container.append(&search);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vexpand(true);
        scroller.set_child(Some(&list));
        container.append(&scroller);

        let actions: Rc<RefCell<HashMap<i32, SidebarRowAction>>> =
            Rc::new(RefCell::new(HashMap::new()));
        self.populate_conversation_switcher_list(&list, &actions, &items);

        let weak_window = self.downgrade();
        let list_for_search = list.clone();
        let actions_for_search = actions.clone();
        let conversations_for_search = conversations.clone();
        let user_names_for_search = user_names.clone();
        search.connect_search_changed(move |entry| {
            if let Some(window) = weak_window.upgrade() {
                let items = conversation_switcher_items(
                    &conversations_for_search,
                    &user_names_for_search,
                    entry.text().as_str(),
                );
                window.populate_conversation_switcher_list(
                    &list_for_search,
                    &actions_for_search,
                    &items,
                );
            }
        });

        let weak_window = self.downgrade();
        let actions_for_activate = actions.clone();
        let dialog_for_activate = dialog.clone();
        list.connect_row_activated(move |_, row| {
            let action = sidebar_row_action_for_index(&actions_for_activate.borrow(), row.index());
            if let (Some(window), Some(action)) = (weak_window.upgrade(), action) {
                window.select_conversation(&action.channel_id, &action.title);
                dialog_for_activate.close();
            }
        });

        dialog.present();
        search.grab_focus();
    }

    fn populate_conversation_switcher_list(
        &self,
        list: &gtk::ListBox,
        actions: &Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
        items: &[SidebarRowModel],
    ) {
        self.clear_list(list);
        actions.borrow_mut().clear();

        if items.is_empty() {
            self.append_placeholder(list, "No matching conversations");
            return;
        }

        for item in items {
            self.append_conversation_switcher_row(list, actions, item);
        }
    }

    fn append_conversation_switcher_row(
        &self,
        list: &gtk::ListBox,
        actions: &Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
        model: &SidebarRowModel,
    ) {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(true);
        row.set_activatable(true);
        let accessible_label = model.accessible_label();
        row.set_tooltip_text(Some(&accessible_label));
        row.update_property(&[gtk::accessible::Property::Label(&accessible_label)]);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(8);
        content.set_margin_end(8);

        let icon = gtk::Image::from_icon_name(model.kind.icon_name());
        icon.set_tooltip_text(Some(model.kind.accessible_name()));
        content.append(&icon);

        let title = gtk::Label::new(Some(&model.title));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        content.append(&title);

        if let Some(unread_label) = model.unread_badge_label() {
            let unread = gtk::Label::new(Some(&unread_label));
            unread.add_css_class("caption");
            unread.add_css_class("heading");
            content.append(&unread);
        }

        row.set_child(Some(&content));
        list.append(&row);
        actions
            .borrow_mut()
            .insert(row.index(), SidebarRowAction::from_model(model));
    }

    fn refresh_current_conversation_title(&self) {
        let imp = self.imp();
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
        let previous_channel = imp.selected_channel.borrow().clone();
        let current_messages = imp.current_channel_messages.borrow().clone();
        let cached_messages = imp.channel_message_cache.borrow().get(channel_id).cloned();
        let fresh_load_in_progress = imp.loading_channel_histories.borrow().contains(channel_id);
        let history_action = conversation_history_selection_action(
            channel_id,
            previous_channel.as_deref(),
            &current_messages,
            cached_messages.as_deref(),
            fresh_load_in_progress,
        );
        if previous_channel.as_deref() != Some(channel_id) {
            self.force_next_channel_bottom_render(channel_id);
        }

        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        *imp.selected_thread_ts.borrow_mut() = None;
        imp.current_main_view.set(MainMessageView::Conversation);
        imp.message_title.set_label(title);
        imp.current_thread_messages.borrow_mut().clear();
        set_text_view_text(&imp.thread_entry, "");
        imp.thread_pane.set_visible(false);
        self.load_thread_html(&message_html::placeholder_document(
            "Thread",
            "No thread open",
        ));
        self.render_conversations();

        match history_action {
            ConversationHistorySelectionAction::RenderCurrent => {
                let scroll_behavior = self.next_channel_history_scroll_behavior(channel_id, false);
                self.populate_history_with_scroll(channel_id, current_messages, scroll_behavior);
            }
            ConversationHistorySelectionAction::RenderCached => {
                if let Some(messages) = cached_messages {
                    let scroll_behavior =
                        self.next_channel_history_scroll_behavior(channel_id, false);
                    self.populate_history_with_scroll(channel_id, messages, scroll_behavior);
                }
            }
            ConversationHistorySelectionAction::RenderCachedAndRefresh => {
                if let Some(messages) = cached_messages {
                    let scroll_behavior =
                        self.next_channel_history_scroll_behavior(channel_id, false);
                    self.populate_history_with_scroll(channel_id, messages, scroll_behavior);
                }
                self.request_channel_history(channel_id);
            }
            ConversationHistorySelectionAction::RequestFresh => {
                self.load_message_html(&message_html::placeholder_document(
                    "Messages",
                    "Loading messages",
                ));
                self.request_channel_history(channel_id);
            }
            ConversationHistorySelectionAction::AwaitFresh => {
                self.load_message_html(&message_html::placeholder_document(
                    "Messages",
                    "Loading messages",
                ));
            }
        }
    }

    fn request_channel_history(&self, channel_id: &str) {
        if !self
            .imp()
            .loading_channel_histories
            .borrow_mut()
            .insert(channel_id.to_string())
        {
            return;
        }

        self.send_command(RuntimeCommand::LoadHistory {
            channel_id: channel_id.to_string(),
        });
    }

    fn force_next_channel_bottom_render(&self, channel_id: &str) {
        self.imp()
            .force_bottom_channel_renders
            .borrow_mut()
            .insert(channel_id.to_string());
    }

    fn next_channel_history_scroll_behavior(
        &self,
        channel_id: &str,
        append_older: bool,
    ) -> TimelineScrollBehavior {
        let force_bottom = self
            .imp()
            .force_bottom_channel_renders
            .borrow_mut()
            .remove(channel_id);
        channel_history_scroll_behavior(append_older, force_bottom)
    }

    fn populate_history(&self, channel_id: &str, messages: Vec<SlackMessage>) {
        self.populate_history_with_scroll(
            channel_id,
            messages,
            TimelineScrollBehavior::StickToBottom,
        );
    }

    fn populate_history_with_scroll(
        &self,
        channel_id: &str,
        messages: Vec<SlackMessage>,
        scroll_behavior: TimelineScrollBehavior,
    ) {
        let imp = self.imp();
        *imp.selected_channel.borrow_mut() = Some(channel_id.to_string());
        imp.current_main_view.set(MainMessageView::Conversation);
        *imp.current_channel_messages.borrow_mut() = messages.clone();
        imp.channel_message_cache
            .borrow_mut()
            .insert(channel_id.to_string(), messages.clone());
        imp.message_title
            .set_label(&self.conversation_title(channel_id));
        self.render_conversations();
        self.request_image_assets(messages.iter());
        let mut context = self.message_html_context(None);
        context.load_more_url = self.channel_load_more_url(channel_id);
        context.timeline_scroll = scroll_behavior;
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
        let opening_thread_pane = !imp.thread_pane.is_visible();
        imp.thread_pane.set_visible(true);
        if opening_thread_pane {
            self.queue_default_thread_pane_position();
        }

        if messages.is_empty() {
            self.load_thread_html(&message_html::placeholder_document("Thread", "No replies"));
            return;
        }

        self.request_image_assets(messages.iter());
        let mut context = self.message_html_context(Some(ts));
        context.load_more_url = self.thread_load_more_url(channel_id, ts);
        self.load_thread_html(&message_html::conversation_document(
            channel_id, &messages, &context,
        ));
    }

    fn queue_default_thread_pane_position(&self) {
        let weak_window = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(window) = weak_window.upgrade() {
                window.set_default_thread_pane_position();
            }
        });
    }

    fn set_default_thread_pane_position(&self) {
        let paned = self.imp().message_thread_paned.get();
        let width = paned.width();
        if width <= 0 {
            return;
        }

        // Keep the thread pane half as wide as the main message pane:
        // message width is 2/3 of the paned area, thread width is 1/3.
        paned.set_position(width * 2 / 3);
    }

    fn populate_activity(&self, items: Vec<ActivityItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Activity");
        imp.current_main_view.set(MainMessageView::Activity);
        self.render_conversations();
        self.load_message_html(&message_html::activity_document(&items));
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_label("Search results");
        imp.current_main_view.set(MainMessageView::Search);
        *imp.current_search_results.borrow_mut() = results.clone();
        let context = self.message_html_context(None);
        self.load_message_html(&message_html::search_results_document(&results, &context));
    }

    fn populate_files(&self, files: Vec<SlackFile>) {
        let imp = self.imp();
        imp.message_title.set_label("Files");
        imp.current_main_view.set(MainMessageView::Files);
        *imp.current_files.borrow_mut() = files.clone();
        self.render_conversations();
        self.load_message_html(&message_html::files_document(&files));
    }

    fn populate_saved_items(&self, items: Vec<SavedItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Later");
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
        let context = self.message_html_context(None);
        self.load_message_html(&message_html::saved_items_document(&items, &context));
    }

    fn notify_if_new_messages(&self, channel_id: &str, messages: &[SlackMessage]) {
        let Some(latest_ts) = SlackMessage::latest_ts(messages.iter()) else {
            return;
        };

        let latest_message = messages.iter().find(|message| message.ts == latest_ts);
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

        let has_new_message = previous_ts
            .as_deref()
            .is_some_and(|previous_ts| latest_ts.as_str() > previous_ts);

        if has_new_message {
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

    fn activity_items(&self) -> Vec<ActivityItem> {
        let imp = self.imp();
        activity::build_activity_items(&imp.conversations.borrow(), &imp.user_names.borrow())
    }

    fn clear_list(&self, list: &gtk::ListBox) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
    }

    fn append_placeholder(&self, list: &gtk::ListBox, text: &str) {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);

        let label = gtk::Label::new(Some(text));
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_xalign(0.0);
        label.add_css_class("dim-label");

        row.set_child(Some(&label));
        list.append(&row);
    }

    fn show_message_placeholder(&self, text: &str) {
        self.load_message_html(&message_html::placeholder_document("Messages", text));
    }

    fn load_message_html(&self, html: &str) {
        if let Some(web_view) = self.imp().message_view.borrow().as_ref() {
            crate::debug::log("ui", &format!("load_message_html bytes={}", html.len()));
            web_view.load_html(html, Some(message_html::base_uri()));
        }
    }

    fn load_thread_html(&self, html: &str) {
        if let Some(web_view) = self.imp().thread_view.borrow().as_ref() {
            crate::debug::log("ui", &format!("load_thread_html bytes={}", html.len()));
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
            MainMessageView::Activity => self.populate_activity(self.activity_items()),
            MainMessageView::Search => self.populate_search_results(snapshot.search_results),
            MainMessageView::Files => self.populate_files(snapshot.files),
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

    fn message_html_context(&self, thread_ts: Option<&str>) -> MessageHtmlContext {
        let imp = self.imp();
        MessageHtmlContext {
            user_names: imp.user_names.borrow().clone(),
            current_user_id: imp.current_user_id.borrow().clone(),
            thread_ts: thread_ts.map(ToString::to_string),
            load_more_url: None,
            timeline_scroll: TimelineScrollBehavior::Preserve,
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
            files: imp.current_files.borrow().clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::ConversationKind;

    fn sidebar_row(id: &str, title: &str) -> SidebarRowModel {
        SidebarRowModel {
            id: id.to_string(),
            title: title.to_string(),
            kind: ConversationKind::DirectMessage,
            unread_count: 0,
            selected: false,
            private: true,
            muted: false,
            external: false,
        }
    }

    fn message(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn sidebar_row_action_uses_conversation_id_and_resolved_title() {
        let model = sidebar_row("D123", "Mohamed Moulay");

        assert_eq!(
            SidebarRowAction::from_model(&model),
            SidebarRowAction {
                channel_id: "D123".to_string(),
                title: "Mohamed Moulay".to_string(),
            }
        );
    }

    #[test]
    fn sidebar_row_action_lookup_ignores_unregistered_rows() {
        let action = SidebarRowAction {
            channel_id: "C123".to_string(),
            title: "#general".to_string(),
        };
        let mut actions = HashMap::new();
        actions.insert(4, action.clone());

        assert_eq!(sidebar_row_action_for_index(&actions, 3), None);
        assert_eq!(sidebar_row_action_for_index(&actions, 4), Some(action));
    }

    #[test]
    fn sidebar_selection_is_visible_only_for_conversation_view() {
        let selected = Some("C123".to_string());

        assert_eq!(
            sidebar_selected_channel(MainMessageView::Conversation, selected.clone()),
            selected
        );
        assert_eq!(
            sidebar_selected_channel(MainMessageView::Search, Some("C123".to_string())),
            None
        );
        assert_eq!(
            sidebar_selected_channel(MainMessageView::Files, Some("C123".to_string())),
            None
        );
        assert_eq!(
            sidebar_selected_channel(MainMessageView::Activity, Some("C123".to_string())),
            None
        );
        assert_eq!(
            sidebar_selected_channel(MainMessageView::Saved, Some("C123".to_string())),
            None
        );
        assert_eq!(
            sidebar_selected_channel(MainMessageView::Placeholder, Some("C123".to_string())),
            None
        );
    }

    #[test]
    fn sidebar_filter_predicate_combines_query_and_unread_toggle() {
        let unread = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            unread_count: Some(2),
            ..Default::default()
        };
        let read = SlackConversation {
            id: "C456".to_string(),
            name: Some("random".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let mut extra_unread = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        extra_unread
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));
        let user_names = HashMap::from([("U123".to_string(), "Ada".to_string())]);

        assert!(sidebar_conversation_matches_filters(
            &unread,
            &user_names,
            "",
            true
        ));
        assert!(!sidebar_conversation_matches_filters(
            &read,
            &user_names,
            "",
            true
        ));
        assert!(sidebar_conversation_matches_filters(
            &extra_unread,
            &user_names,
            "ada",
            true
        ));
        assert!(!sidebar_conversation_matches_filters(
            &unread,
            &user_names,
            "random",
            true
        ));
    }

    #[test]
    fn sidebar_loading_change_rerenders_only_when_list_state_changes() {
        assert!(sidebar_loading_change_needs_render(false, false, true));
        assert!(sidebar_loading_change_needs_render(false, true, false));
        assert!(!sidebar_loading_change_needs_render(true, false, true));
        assert!(!sidebar_loading_change_needs_render(true, true, false));
        assert!(!sidebar_loading_change_needs_render(false, true, true));
    }

    #[test]
    fn sidebar_error_change_preserves_populated_list() {
        assert!(sidebar_error_change_needs_render(false));
        assert!(!sidebar_error_change_needs_render(true));
    }

    #[test]
    fn sidebar_user_name_updates_render_only_for_idle_dm_rows() {
        let dm = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let channel = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let conversations = vec![dm, channel];

        assert!(sidebar_user_name_update_needs_render(
            &conversations,
            "U123",
            false
        ));
        assert!(!sidebar_user_name_update_needs_render(
            &conversations,
            "U999",
            false
        ));
        assert!(!sidebar_user_name_update_needs_render(
            &conversations,
            "U123",
            true
        ));
        assert!(!sidebar_user_name_update_needs_render(&[], "U123", false));
    }

    #[test]
    fn conversation_switcher_items_search_all_loaded_conversations() {
        let active = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let dormant_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D123",
            "user": "U123",
            "is_im": true,
            "properties": {
                "is_dormant": true
            }
        }))
        .expect("failed to parse dormant DM");
        let user_names = HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]);

        let items = conversation_switcher_items(&[active, dormant_dm], &user_names, "ada");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "D123");
        assert_eq!(items[0].title, "Ada Lovelace");
    }

    #[test]
    fn conversation_switcher_items_match_title_and_id() {
        let general = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let random = SlackConversation {
            id: "C456".to_string(),
            name: Some("random".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };

        let title_match =
            conversation_switcher_items(&[general.clone(), random.clone()], &HashMap::new(), "gen");
        let id_match = conversation_switcher_items(&[general, random], &HashMap::new(), "456");

        assert_eq!(title_match[0].id, "C123");
        assert_eq!(id_match[0].id, "C456");
    }

    #[test]
    fn browser_session_input_requires_both_tokens() {
        assert_eq!(
            browser_session_input("xoxc-token", "").unwrap_err(),
            "Enter XOXC and XOXD tokens"
        );
        assert_eq!(
            browser_session_input("", "xoxd-token").unwrap_err(),
            "Enter XOXC and XOXD tokens"
        );
    }

    #[test]
    fn browser_session_input_trims_token_values() {
        assert_eq!(
            browser_session_input(" xoxc-token ", " xoxd-token ").unwrap(),
            ("xoxc-token".to_string(), "xoxd-token".to_string())
        );
    }

    #[test]
    fn merge_message_pages_deduplicates_and_sorts_newest_first() {
        let existing = vec![
            message("1710000300.000000", "new"),
            message("1710000200.000000", "middle"),
        ];
        let page = vec![
            message("1710000200.000000", "duplicate"),
            message("1710000100.000000", "old"),
        ];

        let merged = merge_message_pages(&existing, &page);
        let timestamps = merged
            .iter()
            .map(|message| message.ts.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            timestamps,
            vec![
                "1710000300.000000",
                "1710000200.000000",
                "1710000100.000000"
            ]
        );
    }

    #[test]
    fn conversation_history_selection_reuses_current_or_cached_messages() {
        let current = vec![message("1710000300.000000", "current")];
        let cached = vec![message("1710000200.000000", "cached")];

        assert_eq!(
            conversation_history_selection_action("C1", Some("C1"), &current, None, false),
            ConversationHistorySelectionAction::RenderCurrent
        );
        assert_eq!(
            conversation_history_selection_action("C2", Some("C1"), &current, Some(&cached), false),
            ConversationHistorySelectionAction::RenderCachedAndRefresh
        );
        assert_eq!(
            conversation_history_selection_action("C2", Some("C1"), &current, Some(&[]), false),
            ConversationHistorySelectionAction::RequestFresh
        );
    }

    #[test]
    fn conversation_history_selection_avoids_duplicate_fresh_loads() {
        let current = vec![message("1710000300.000000", "current")];
        let cached = vec![message("1710000200.000000", "cached")];

        assert_eq!(
            conversation_history_selection_action("C2", Some("C1"), &current, Some(&cached), true),
            ConversationHistorySelectionAction::RenderCached
        );
        assert_eq!(
            conversation_history_selection_action("C2", Some("C1"), &current, Some(&[]), true),
            ConversationHistorySelectionAction::AwaitFresh
        );
    }

    #[test]
    fn cached_history_events_do_not_update_fresh_metadata_or_read_state() {
        assert!(!history_event_updates_fresh_metadata(true));
        assert!(!history_event_marks_read(true, false));
        assert!(!history_event_marks_read(true, true));

        assert!(history_event_updates_fresh_metadata(false));
        assert!(history_event_marks_read(false, false));
        assert!(!history_event_marks_read(false, true));
    }

    #[test]
    fn channel_history_scroll_behavior_forces_bottom_for_explicit_bottom_renders() {
        assert_eq!(
            channel_history_scroll_behavior(false, true),
            message_html::TimelineScrollBehavior::Bottom
        );
    }

    #[test]
    fn channel_history_scroll_behavior_sticks_only_when_already_bottom_for_updates() {
        assert_eq!(
            channel_history_scroll_behavior(false, false),
            message_html::TimelineScrollBehavior::StickToBottom
        );
    }

    #[test]
    fn channel_history_scroll_behavior_preserves_prepended_older_pages() {
        assert_eq!(
            channel_history_scroll_behavior(true, false),
            message_html::TimelineScrollBehavior::PreservePrepend
        );
        assert_eq!(
            channel_history_scroll_behavior(true, true),
            message_html::TimelineScrollBehavior::PreservePrepend
        );
    }
}
