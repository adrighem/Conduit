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

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::activity::{self, ActivityItem};
use crate::auth;
use crate::composer::{
    set_text_view_text, text_view_enter_action, text_view_text, TextViewEnterAction,
};
use crate::config;
use crate::message_html::{self, MessageHtmlContext, TimelineScrollBehavior};
use crate::models::{
    SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackUnreadState,
};
use crate::rendering;
use crate::runtime::{
    AppRuntime, OperationContext, RequestId, RuntimeCommand, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimeIdentity, SessionId,
};
use crate::shortcuts::WINDOW_SHORTCUTS;
use crate::sidebar::{self, SidebarRowModel, SidebarSectionModel};
use crate::sidebar_widgets::{sidebar_row_widget, SidebarRowLayout};
use crate::socket_mode::{
    SocketModeEvent, SocketModeMessageEvent, SocketModeMessageKind, SocketModeReactionEvent,
};
use crate::workspace_state::{
    ConversationSelectionDecision, MainMessageView, ReactionUpdate, RealtimeMessageKind,
    ThreadApplyOutcome, ThreadOpenOutcome, WorkspaceScrollBehavior, WorkspaceSnapshot,
    WorkspaceViewState,
};

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
        pub(super) request_coordinator: RefCell<RequestCoordinator>,
        pub settings: RefCell<Option<gio::Settings>>,
        pub connect_requested: Cell<bool>,
        pub auth_debug: Cell<bool>,
        pub conversations: RefCell<Vec<SlackConversation>>,
        pub(super) sidebar_row_actions: RefCell<HashMap<i32, SidebarRowAction>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub user_group_names: RefCell<HashMap<String, String>>,
        pub user_group_members: RefCell<HashMap<String, Vec<String>>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub workspace_name: RefCell<Option<String>>,
        pub workspace_url: RefCell<Option<String>>,
        pub sidebar_loading: Cell<bool>,
        pub sidebar_error: RefCell<Option<String>>,
        pub(super) workspace_view: RefCell<WorkspaceViewState>,
        pub current_user_id: RefCell<Option<String>>,
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
            obj.setup_settings();
            obj.setup_callbacks();
            obj.show_loading("Checking secure storage");
            obj.send_session_command(RuntimeCommand::LoadStoredToken);
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

#[derive(Debug, Default)]
struct RequestCoordinator {
    session: SessionId,
    next_request: u64,
    latest: HashMap<OperationContext, RequestId>,
}

impl RequestCoordinator {
    fn issue(&mut self, command: &RuntimeCommand) -> RuntimeIdentity {
        self.next_request = self.next_request.saturating_add(1);
        let request = RequestId::new(self.next_request);
        if command.supersedes_previous() {
            self.latest.insert(command.operation_context(), request);
        }
        RuntimeIdentity {
            session: self.session,
            request,
        }
    }

    fn begin_session(&mut self, command: &RuntimeCommand) -> RuntimeIdentity {
        self.invalidate_session();
        self.issue(command)
    }

    fn invalidate_session(&mut self) {
        self.session = self.session.next();
        self.latest.clear();
    }

    fn accepts(&self, meta: &RuntimeEventMeta) -> bool {
        meta.session == self.session
            && meta.request.is_none_or(|request| {
                self.latest
                    .get(&meta.context)
                    .is_none_or(|latest| *latest == request)
            })
    }
}

fn runtime_event_is_start_failure(event: &RuntimeEvent) -> bool {
    matches!(event.kind, RuntimeEventKind::RuntimeStartFailed(_))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageNotificationAction {
    Notify,
    RecordOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MessageNotificationState<'a> {
    channel_id: &'a str,
    selected_channel: Option<&'a str>,
    previous_latest_ts: Option<&'a str>,
    latest_ts: &'a str,
    latest_message_user: Option<&'a str>,
    current_user: Option<&'a str>,
    has_unread: bool,
    muted: bool,
}

fn message_notification_action(state: MessageNotificationState<'_>) -> MessageNotificationAction {
    let has_newer_message = state
        .previous_latest_ts
        .is_some_and(|previous_ts| state.latest_ts > previous_ts);
    let selected = state.selected_channel == Some(state.channel_id);
    let own_message = state
        .latest_message_user
        .is_some_and(|user| Some(user) == state.current_user);

    if has_newer_message && state.has_unread && !state.muted && !selected && !own_message {
        MessageNotificationAction::Notify
    } else {
        MessageNotificationAction::RecordOnly
    }
}

fn message_notification_body(message: Option<&SlackMessage>) -> &str {
    message
        .and_then(|message| message.text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("New message")
}

fn conversation_refresh_start_shows_sidebar_loading() -> bool {
    false
}

fn sidebar_error_change_needs_render(has_conversations: bool) -> bool {
    !has_conversations
}

fn connected_workspace_status(workspace_name: Option<&str>) -> String {
    format!("Connected to {}", workspace_name.unwrap_or("Slack"))
}

fn sidebar_user_name_update_needs_render(
    conversations: &[SlackConversation],
    user_id: &str,
    sidebar_loading: bool,
) -> bool {
    !sidebar_loading
        && conversations.iter().any(|conversation| {
            conversation
                .display_user_ids()
                .iter()
                .any(|display_user_id| display_user_id == user_id)
        })
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

const THREAD_PANE_MIN_WIDTH: i32 = 380;
const THREAD_PANE_MAX_WIDTH: i32 = 500;
const MAIN_PANE_MIN_WITH_THREAD: i32 = 440;

fn default_thread_pane_position(width: i32) -> Option<i32> {
    if width <= 0 {
        return None;
    }

    let thread_width = if width < MAIN_PANE_MIN_WITH_THREAD + THREAD_PANE_MIN_WIDTH {
        width / 2
    } else {
        let responsive_width = width * 2 / 5;
        let max_thread_width = THREAD_PANE_MAX_WIDTH.min(width - MAIN_PANE_MIN_WITH_THREAD);
        responsive_width.clamp(THREAD_PANE_MIN_WIDTH, max_thread_width)
    };

    Some(width - thread_width)
}

fn realtime_message_marks_unread(
    reading_channel: Option<&str>,
    current_user_id: Option<&str>,
    event: &SocketModeMessageEvent,
) -> bool {
    event.kind == SocketModeMessageKind::Posted
        && reading_channel != Some(event.channel_id.as_str())
        && event
            .message
            .user
            .as_deref()
            .is_none_or(|user| Some(user) != current_user_id)
}

fn mutation_completion_reloads_visible_channel(
    visible_channel: Option<&str>,
    completed_channel: &str,
) -> bool {
    visible_channel == Some(completed_channel)
}

fn timeline_scroll_behavior(behavior: WorkspaceScrollBehavior) -> TimelineScrollBehavior {
    match behavior {
        WorkspaceScrollBehavior::Preserve => TimelineScrollBehavior::Preserve,
        WorkspaceScrollBehavior::PreservePrepend => TimelineScrollBehavior::PreservePrepend,
        WorkspaceScrollBehavior::StickToBottom => TimelineScrollBehavior::StickToBottom,
        WorkspaceScrollBehavior::Bottom => TimelineScrollBehavior::Bottom,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MessageWebViewFeaturePolicy {
    javascript: bool,
    html5_local_storage: bool,
    file_url_access: bool,
    universal_file_url_access: bool,
    media: bool,
    webgl: bool,
    webaudio: bool,
}

fn message_web_view_feature_policy() -> MessageWebViewFeaturePolicy {
    MessageWebViewFeaturePolicy {
        javascript: true,
        html5_local_storage: true,
        file_url_access: false,
        universal_file_url_access: false,
        media: false,
        webgl: false,
        webaudio: false,
    }
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
        let (runtime, mut events) = AppRuntime::start();

        *imp.runtime.borrow_mut() = Some(runtime.clone());

        let weak_window = self.downgrade();
        glib::spawn_future_local(async move {
            let mut startup_failed = false;
            while let Some(event) = events.recv().await {
                let Some(window) = weak_window.upgrade() else {
                    return;
                };
                startup_failed |= runtime_event_is_start_failure(&event);
                window.handle_runtime_event(event);
            }
            if !startup_failed {
                let Some(window) = weak_window.upgrade() else {
                    return;
                };
                window.show_error("Background runtime stopped");
            }
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
        let features = message_web_view_feature_policy();
        settings.set_allow_file_access_from_file_urls(features.file_url_access);
        settings.set_allow_universal_access_from_file_urls(features.universal_file_url_access);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(features.html5_local_storage);
        settings.set_enable_javascript(features.javascript);
        settings.set_enable_media(features.media);
        settings.set_enable_webgl(features.webgl);
        settings.set_enable_webaudio(features.webaudio);

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

    fn setup_settings(&self) {
        let settings = gio::Settings::new(config::APPLICATION_ID);
        let weak_window = self.downgrade();
        settings.connect_changed(
            Some(config::SIDEBAR_SHOW_UNREADS_SECTION_KEY),
            move |_, _| {
                if let Some(window) = weak_window.upgrade() {
                    window.render_conversations();
                }
            },
        );
        *self.imp().settings.borrow_mut() = Some(settings);
    }

    fn setup_window_actions(&self) {
        self.add_window_action("sign-out", |window| {
            window.send_session_command(RuntimeCommand::SignOut)
        });
        self.add_window_action("switch-conversation", |window| {
            window.show_conversation_switcher()
        });
        self.add_window_action("search-workspace", |window| window.focus_workspace_search());
        self.add_window_action("go-home", |window| window.show_home());
        self.add_window_action("show-activity", |window| window.show_activity());
        self.add_window_action("show-files", |window| window.show_files());
        self.add_window_action("show-later", |window| window.show_later());
        self.add_window_action("refresh-conversations", |window| {
            window.refresh_conversations()
        });
        self.add_window_action("focus-composer", |window| window.focus_composer());
        self.add_window_action("upload-file", |window| window.choose_file_for_upload());
        self.add_window_action("close-thread", |window| window.close_thread());

        if let Some(application) = self.application() {
            for shortcut in WINDOW_SHORTCUTS {
                application.set_accels_for_action(shortcut.action, shortcut.accelerators);
            }
        }
    }

    fn add_window_action<F>(&self, name: &str, callback: F)
    where
        F: Fn(&Self) + 'static,
    {
        let action = gio::SimpleAction::new(name, None);
        let weak_window = self.downgrade();
        action.connect_activate(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                callback(&window);
            }
        });
        self.add_action(&action);
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
            if text_view_enter_action(key, state) == TextViewEnterAction::Send {
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

    fn handle_runtime_event(&self, event: RuntimeEvent) {
        let RuntimeEvent { meta, kind } = event;
        if !self.imp().request_coordinator.borrow().accepts(&meta) {
            crate::debug::log(
                "ui",
                &format!(
                    "RuntimeEventIgnored reason=stale session={:?} request={:?} operation={:?}",
                    meta.session, meta.request, meta.context.operation
                ),
            );
            return;
        }

        match kind {
            RuntimeEventKind::Status(status) => {
                if !self.imp().connect_requested.get() {
                    if status == "Loading conversations"
                        && conversation_refresh_start_shows_sidebar_loading()
                    {
                        self.start_sidebar_loading();
                    }
                    self.set_status(&status);
                }
            }
            RuntimeEventKind::Error(error) => self.show_error(&error),
            RuntimeEventKind::RuntimeStartFailed(error) => self.show_error(&error),
            RuntimeEventKind::SignedOut => {
                self.imp().connect_requested.set(false);
                self.show_login("Choose a workspace to continue");
            }
            RuntimeEventKind::Authenticated(auth) => {
                if !self.imp().connect_requested.get() {
                    self.show_workspace(auth);
                }
            }
            RuntimeEventKind::ConversationsLoaded(conversations) => {
                if !self.imp().connect_requested.get() {
                    self.populate_conversations(conversations);
                    self.restore_workspace_status();
                }
            }
            RuntimeEventKind::ConversationsLoadFailed(error) => {
                if !self.imp().connect_requested.get() {
                    self.show_conversation_load_error(&error);
                }
            }
            RuntimeEventKind::ConversationUnreadUpdated {
                channel_id,
                unread_state,
            } => self.apply_conversation_unread_state(&channel_id, unread_state),
            RuntimeEventKind::ConversationNotificationCandidate {
                channel_id,
                messages,
            } => self.notify_if_new_messages(&channel_id, &messages),
            RuntimeEventKind::HistoryLoaded {
                channel_id,
                messages,
                has_more,
                next_cursor,
                append_older,
                cached,
            } => {
                let outcome = self.imp().workspace_view.borrow_mut().apply_history(
                    &channel_id,
                    messages,
                    has_more,
                    next_cursor,
                    append_older,
                    cached,
                );
                if outcome.visible {
                    let rendered_messages = self
                        .imp()
                        .workspace_view
                        .borrow()
                        .snapshot()
                        .channel_messages;
                    if outcome.mark_read {
                        self.notify_if_new_messages(&channel_id, &rendered_messages);
                        self.mark_conversation_locally_read(&channel_id);
                    }
                    self.populate_history_with_scroll(
                        &channel_id,
                        rendered_messages,
                        timeline_scroll_behavior(
                            outcome
                                .scroll
                                .unwrap_or(WorkspaceScrollBehavior::StickToBottom),
                        ),
                    );
                    if !cached {
                        self.restore_workspace_status();
                    }
                }
            }
            RuntimeEventKind::ThreadLoaded {
                channel_id,
                ts,
                messages,
                has_more,
                next_cursor,
                append_older,
            } => {
                let outcome = self.imp().workspace_view.borrow_mut().apply_thread(
                    &channel_id,
                    &ts,
                    messages,
                    has_more,
                    next_cursor,
                    append_older,
                );
                if let ThreadApplyOutcome::Applied { scroll } = outcome {
                    let rendered_messages = self
                        .imp()
                        .workspace_view
                        .borrow()
                        .snapshot()
                        .thread_messages;
                    self.request_user_names(&rendered_messages);
                    self.populate_thread(
                        &channel_id,
                        &ts,
                        rendered_messages,
                        timeline_scroll_behavior(scroll),
                    );
                    self.restore_workspace_status();
                }
            }
            RuntimeEventKind::SearchLoaded(results) => {
                let visible = self
                    .imp()
                    .workspace_view
                    .borrow_mut()
                    .apply_search_results(results);
                if visible {
                    let results = self.imp().workspace_view.borrow().search_results().to_vec();
                    self.populate_search_results(results);
                }
            }
            RuntimeEventKind::FilesLoaded(files) => {
                let visible = self.imp().workspace_view.borrow_mut().apply_files(files);
                if visible {
                    let files = self.imp().workspace_view.borrow().files().to_vec();
                    self.populate_files(files);
                }
            }
            RuntimeEventKind::SavedItemsLoaded(items) => {
                let visible = self.imp().workspace_view.borrow_mut().apply_saved(items);
                if visible {
                    let items = self.imp().workspace_view.borrow().saved_items().to_vec();
                    self.populate_saved_items(items);
                }
            }
            RuntimeEventKind::SocketModeEvent(event) => self.handle_socket_mode_event(event),
            RuntimeEventKind::UserLoaded {
                user_id,
                display_name,
            } => {
                self.populate_user_names(HashMap::from([(user_id, display_name)]));
            }
            RuntimeEventKind::UserNamesLoaded(user_names) => self.populate_user_names(user_names),
            RuntimeEventKind::UserGroupsLoaded { names, members } => {
                self.populate_user_groups(names, members);
            }
            RuntimeEventKind::ImageAssetLoaded { key, data_uri } => {
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
            RuntimeEventKind::ImageAssetFailed { key } => {
                crate::debug::log(
                    "ui",
                    &format!("ImageAssetFailed key={}", crate::debug::url_for_log(&key)),
                );
                let imp = self.imp();
                imp.pending_image_assets.borrow_mut().remove(&key);
                imp.failed_image_assets.borrow_mut().insert(key);
                self.rerender_current_messages();
            }
            RuntimeEventKind::MessagePosted {
                channel_id,
                message,
            } => {
                set_text_view_text(&self.imp().message_entry, "");
                self.imp().send_button.set_sensitive(true);
                self.imp().thread_send_button.set_sensitive(true);
                self.set_status("Message sent");
                let thread_ts = message.thread_ts.clone();
                if let Some(thread_ts) = thread_ts.as_deref() {
                    self.note_thread_reply_posted(&channel_id, thread_ts);
                } else {
                    self.force_next_channel_bottom_render(&channel_id);
                }
                self.reload_after_message(&channel_id, thread_ts.as_deref());
            }
            RuntimeEventKind::ReactionUpdated {
                channel_id,
                thread_ts,
            } => {
                self.set_status("Reaction updated");
                self.reload_after_message(&channel_id, thread_ts.as_deref());
            }
            RuntimeEventKind::SavedUpdated {
                channel_id,
                saved,
                thread_ts,
            } => {
                self.set_status(if saved {
                    "Saved for later"
                } else {
                    "Removed from saved items"
                });
                if self.current_main_view() == MainMessageView::Saved {
                    self.send_command(RuntimeCommand::LoadSavedItems);
                } else {
                    self.reload_after_message(&channel_id, thread_ts.as_deref());
                }
            }
            RuntimeEventKind::FileUploadProgress { fraction, label } => {
                let imp = self.imp();
                imp.upload_progress.set_visible(true);
                imp.upload_progress.set_fraction(fraction);
                imp.upload_progress.set_text(Some(&label));
                self.set_status(&label);
            }
            RuntimeEventKind::FileUploaded(name) => {
                let imp = self.imp();
                imp.upload_button.set_sensitive(true);
                imp.upload_progress.set_fraction(1.0);
                imp.upload_progress.set_text(Some("Upload complete"));
                set_text_view_text(&imp.message_entry, "");
                self.set_status(&format!("Uploaded {name}"));
                if let Some(channel_id) = self.visible_channel_id() {
                    self.force_next_channel_bottom_render(&channel_id);
                    self.request_channel_history(&channel_id);
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
        self.send_session_command(RuntimeCommand::StartOAuth {
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
        self.send_session_command(RuntimeCommand::StartBrowserSession {
            xoxc_token,
            xoxd_token,
            user_agent,
        });
    }

    fn refresh_conversations(&self) {
        if conversation_refresh_start_shows_sidebar_loading() {
            self.start_sidebar_loading();
        }
        self.send_command(RuntimeCommand::RefreshConversations);
    }

    fn show_home(&self) {
        if let Some(channel_id) = self.selected_channel_id() {
            let title = self.conversation_title(&channel_id);
            self.select_conversation(&channel_id, &title);
        } else {
            self.imp().workspace_view.borrow_mut().show_placeholder();
            self.imp().message_title.set_label("Select a conversation");
            self.show_message_placeholder("Select a conversation");
            self.render_closed_thread();
            self.render_conversations();
        }
    }

    fn show_activity(&self) {
        self.imp().workspace_view.borrow_mut().show_activity();
        self.render_closed_thread();
        let items = self.activity_items();
        self.populate_activity(items);
    }

    fn show_files(&self) {
        self.imp().workspace_view.borrow_mut().start_files();
        self.render_closed_thread();
        self.imp().message_title.set_label("Files");
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(
            "Files",
            "Loading files",
        ));
        self.send_command(RuntimeCommand::LoadFiles);
    }

    fn show_later(&self) {
        self.imp().workspace_view.borrow_mut().start_saved();
        self.imp().message_title.set_label("Later");
        self.render_closed_thread();
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
        self.imp().workspace_view.borrow_mut().start_search();
        self.render_closed_thread();
        self.render_conversations();
        self.imp().message_title.set_label("Search results");
        self.load_message_html(&message_html::placeholder_document(
            "Search results",
            "Searching",
        ));
        self.send_command(RuntimeCommand::SearchMessages { query });
    }

    fn focus_workspace_search(&self) {
        let entry = self.imp().message_search_entry.get();
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    fn focus_composer(&self) {
        let imp = self.imp();
        if imp.thread_pane.is_visible() {
            imp.thread_entry.grab_focus();
        } else if self.visible_channel_id().is_some() {
            imp.message_entry.grab_focus();
        } else {
            self.set_status("Select a conversation");
        }
    }

    fn post_current_message(&self) {
        let imp = self.imp();
        let Some(channel_id) = self.visible_channel_id() else {
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
        let Some(channel_id) = self.visible_channel_id() else {
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
        let Some(channel_id) = self.visible_channel_id() else {
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
        self.imp().workspace_view.borrow_mut().close_thread();
        self.render_closed_thread();
    }

    fn render_closed_thread(&self) {
        let imp = self.imp();
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
                if self.visible_channel_id().as_deref() != Some(channel_id.as_str()) {
                    let title = self.conversation_title(&channel_id);
                    self.select_conversation(&channel_id, &title);
                }
                let outcome = self
                    .imp()
                    .workspace_view
                    .borrow_mut()
                    .open_thread(&channel_id, &ts);
                match outcome {
                    ThreadOpenOutcome::RenderCurrent => {
                        let messages = self
                            .imp()
                            .workspace_view
                            .borrow()
                            .current_thread_messages()
                            .to_vec();
                        self.populate_thread(
                            &channel_id,
                            &ts,
                            messages,
                            TimelineScrollBehavior::StickToBottom,
                        );
                    }
                    ThreadOpenOutcome::RequestFresh => {
                        self.set_status("Loading thread");
                        self.send_command(RuntimeCommand::LoadThread { channel_id, ts });
                    }
                    ThreadOpenOutcome::AwaitFresh => self.set_status("Loading thread"),
                    ThreadOpenOutcome::Ignored => {}
                }
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
        self.imp()
            .workspace_view
            .borrow()
            .find_message(channel_id, ts)
    }

    fn note_thread_reply_posted(&self, channel_id: &str, thread_ts: &str) {
        let should_render = {
            let mut state = self.imp().workspace_view.borrow_mut();
            state.increment_thread_reply(channel_id, thread_ts)
                && state.visible_channel_id() == Some(channel_id)
        };
        if should_render {
            let messages = self
                .imp()
                .workspace_view
                .borrow()
                .channel_messages(channel_id)
                .to_vec();
            self.populate_history_with_scroll(
                channel_id,
                messages,
                TimelineScrollBehavior::Preserve,
            );
        }
    }

    fn reload_after_message(&self, channel_id: &str, thread_ts: Option<&str>) {
        if let Some(thread_ts) = thread_ts {
            set_text_view_text(&self.imp().thread_entry, "");
            let should_load = {
                let mut state = self.imp().workspace_view.borrow_mut();
                state.visible_channel_id() == Some(channel_id)
                    && state.selected_thread_ts() == Some(thread_ts)
                    && state.begin_thread_history_request()
            };
            if should_load {
                self.send_command(RuntimeCommand::LoadThread {
                    channel_id: channel_id.to_string(),
                    ts: thread_ts.to_string(),
                });
            }
        } else {
            let visible_channel = self.visible_channel_id();
            if mutation_completion_reloads_visible_channel(visible_channel.as_deref(), channel_id) {
                self.request_channel_history(channel_id);
            }
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
        self.send_session_command(RuntimeCommand::Disconnect);
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
        imp.workspace_view.borrow_mut().reset();
        *imp.current_user_id.borrow_mut() = None;
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.conversations.borrow_mut().clear();
        imp.sidebar_row_actions.borrow_mut().clear();
        imp.user_names.borrow_mut().clear();
        imp.user_group_names.borrow_mut().clear();
        imp.user_group_members.borrow_mut().clear();
        imp.pending_user_ids.borrow_mut().clear();
        *imp.workspace_name.borrow_mut() = None;
        *imp.workspace_url.borrow_mut() = None;
        imp.sidebar_loading.set(false);
        *imp.sidebar_error.borrow_mut() = None;
        imp.image_assets.borrow_mut().clear();
        imp.pending_image_assets.borrow_mut().clear();
        imp.failed_image_assets.borrow_mut().clear();
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
        self.set_status(&connected_workspace_status(Some(&workspace_name)));
        self.imp().content_stack.set_visible_child_name("workspace");
        if conversation_refresh_start_shows_sidebar_loading() {
            self.start_sidebar_loading();
        }
    }

    fn set_status(&self, status: &str) {
        let imp = self.imp();
        imp.status_label.set_label(status);
        imp.connection_label.set_label(status);
        imp.workspace_status_label.set_label(status);
    }

    fn restore_workspace_status(&self) {
        let workspace_name = self.imp().workspace_name.borrow().clone();
        self.set_status(&connected_workspace_status(workspace_name.as_deref()));
    }

    fn start_sidebar_loading(&self) {
        let imp = self.imp();
        if !imp.sidebar_loading.replace(true) {
            *imp.sidebar_error.borrow_mut() = None;
            if imp.conversations.borrow().is_empty() {
                self.render_conversations();
            }
        }
    }

    fn show_error(&self, error: &str) {
        self.imp().send_button.set_sensitive(true);
        self.imp().thread_send_button.set_sensitive(true);
        self.imp().upload_button.set_sensitive(true);
        self.imp().upload_progress.set_visible(false);
        self.imp()
            .workspace_view
            .borrow_mut()
            .clear_history_loading();
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
        if self.current_main_view() == MainMessageView::Activity {
            self.populate_activity(self.activity_items());
        } else {
            self.refresh_current_conversation_title();
        }
    }

    fn populate_user_names(&self, user_names: HashMap<String, String>) {
        if user_names.is_empty() {
            return;
        }

        let changed_user_ids = {
            let imp = self.imp();
            let mut known_user_names = imp.user_names.borrow_mut();
            let mut pending_user_ids = imp.pending_user_ids.borrow_mut();
            let mut changed_user_ids = Vec::new();

            for (user_id, display_name) in user_names {
                if user_id.trim().is_empty() || display_name.trim().is_empty() {
                    continue;
                }
                pending_user_ids.remove(&user_id);
                if known_user_names.get(&user_id) != Some(&display_name) {
                    known_user_names.insert(user_id.clone(), display_name);
                    changed_user_ids.push(user_id);
                }
            }

            changed_user_ids
        };

        if changed_user_ids.is_empty() {
            return;
        }

        let should_render_sidebar = {
            let imp = self.imp();
            let conversations = imp.conversations.borrow();
            changed_user_ids.iter().any(|user_id| {
                sidebar_user_name_update_needs_render(
                    &conversations,
                    user_id,
                    imp.sidebar_loading.get(),
                )
            })
        };
        if should_render_sidebar {
            self.render_conversations();
        }
        self.refresh_current_conversation_title();
        self.rerender_current_messages();
    }

    fn populate_user_groups(
        &self,
        names: HashMap<String, String>,
        members: HashMap<String, Vec<String>>,
    ) {
        if names.is_empty() && members.is_empty() {
            return;
        }

        let changed = {
            let imp = self.imp();
            let mut known_names = imp.user_group_names.borrow_mut();
            let mut known_members = imp.user_group_members.borrow_mut();
            let mut changed = false;

            for (group_id, name) in names {
                if group_id.trim().is_empty() || name.trim().is_empty() {
                    continue;
                }
                if known_names.get(&group_id) != Some(&name) {
                    known_names.insert(group_id, name);
                    changed = true;
                }
            }

            for (group_id, member_names) in members {
                if group_id.trim().is_empty() {
                    continue;
                }
                if known_members.get(&group_id) != Some(&member_names) {
                    known_members.insert(group_id, member_names);
                    changed = true;
                }
            }

            changed
        };

        if changed {
            self.rerender_current_messages();
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

    fn apply_conversation_unread_state(&self, channel_id: &str, unread_state: SlackUnreadState) {
        if !unread_state.known {
            return;
        }
        if self
            .visible_channel_id()
            .as_deref()
            .is_some_and(|selected_channel| selected_channel == channel_id)
        {
            return;
        }

        let changed = {
            let mut conversations = self.imp().conversations.borrow_mut();
            let Some(conversation) = conversations
                .iter_mut()
                .find(|conversation| conversation.id == channel_id)
            else {
                return;
            };

            let previous_unread = conversation.has_unread_activity();
            let previous_count = conversation.unread_activity_count();
            conversation.apply_unread_state(unread_state);
            previous_unread != conversation.has_unread_activity()
                || previous_count != conversation.unread_activity_count()
        };

        if changed {
            self.render_conversations();
            if self.current_main_view() == MainMessageView::Activity {
                self.populate_activity(self.activity_items());
            }
        }
    }

    fn mark_conversation_locally_unread(&self, channel_id: &str) -> bool {
        let mut conversations = self.imp().conversations.borrow_mut();
        let Some(conversation) = conversations
            .iter_mut()
            .find(|conversation| conversation.id == channel_id)
        else {
            return false;
        };

        let unread_count = conversation.unread_activity_count().saturating_add(1);
        conversation.unread_count = Some(unread_count);
        conversation
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));
        true
    }

    fn channel_load_more_url(&self, channel_id: &str) -> Option<String> {
        self.imp()
            .workspace_view
            .borrow()
            .channel_cursor(channel_id)
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, None))
    }

    fn thread_load_more_url(&self, channel_id: &str, ts: &str) -> Option<String> {
        self.imp()
            .workspace_view
            .borrow()
            .thread_cursor()
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, Some(ts)))
    }

    fn render_conversations(&self) {
        let imp = self.imp();
        let conversations = imp.conversations.borrow().clone();
        let user_names = imp.user_names.borrow().clone();
        let selected_channel = self.visible_channel_id();
        let model = sidebar::build_sidebar_list(
            &conversations,
            &user_names,
            sidebar::SidebarBuildOptions {
                selected_channel: selected_channel.as_deref(),
                query: imp.sidebar_filter_entry.text().as_str(),
                unread_only: imp.sidebar_unread_filter_button.is_active(),
                show_unreads_section: self.show_unreads_section(),
                show_all: imp.sidebar_all_filter_button.is_active(),
                loading: imp.sidebar_loading.get(),
                has_error: imp.sidebar_error.borrow().is_some(),
            },
        );

        imp.sidebar_row_actions.borrow_mut().clear();
        self.clear_list(&imp.conversation_list);

        match model {
            sidebar::SidebarListModel::Placeholder(placeholder) => {
                self.append_placeholder(&imp.conversation_list, placeholder.label());
            }
            sidebar::SidebarListModel::Sections(sections) => {
                for section in sections {
                    self.append_sidebar_section(&imp.conversation_list, &section);
                }
            }
        }
    }

    fn show_unreads_section(&self) -> bool {
        self.imp()
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.boolean(config::SIDEBAR_SHOW_UNREADS_SECTION_KEY))
            .unwrap_or(false)
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
        let row = sidebar_row_widget(model, SidebarRowLayout::sidebar());
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
        let items = sidebar::conversation_switcher_items(&conversations, &user_names, "");
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
                let items = sidebar::conversation_switcher_items(
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
        let row = sidebar_row_widget(model, SidebarRowLayout::switcher());
        list.append(&row);
        actions
            .borrow_mut()
            .insert(row.index(), SidebarRowAction::from_model(model));
    }

    fn refresh_current_conversation_title(&self) {
        let imp = self.imp();
        if self.current_main_view() == MainMessageView::Conversation {
            if let Some(channel_id) = self.visible_channel_id() {
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
        let outcome = imp
            .workspace_view
            .borrow_mut()
            .select_conversation(channel_id);
        let current_messages = imp.workspace_view.borrow().snapshot().channel_messages;
        imp.message_title.set_label(title);
        set_text_view_text(&imp.thread_entry, "");
        imp.thread_pane.set_visible(false);
        self.load_thread_html(&message_html::placeholder_document(
            "Thread",
            "No thread open",
        ));
        self.render_conversations();

        match outcome.decision {
            ConversationSelectionDecision::RenderCurrent
            | ConversationSelectionDecision::RenderCached
            | ConversationSelectionDecision::RenderCachedAndRefresh => {
                self.populate_history_with_scroll(
                    channel_id,
                    current_messages,
                    timeline_scroll_behavior(
                        outcome
                            .scroll
                            .unwrap_or(WorkspaceScrollBehavior::StickToBottom),
                    ),
                );
                if outcome.decision.requests_history() {
                    self.send_command(RuntimeCommand::LoadHistory {
                        channel_id: channel_id.to_string(),
                    });
                }
            }
            ConversationSelectionDecision::RequestFresh => {
                self.load_message_html(&message_html::placeholder_document(
                    "Messages",
                    "Loading messages",
                ));
                self.send_command(RuntimeCommand::LoadHistory {
                    channel_id: channel_id.to_string(),
                });
            }
            ConversationSelectionDecision::AwaitFresh => {
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
            .workspace_view
            .borrow_mut()
            .begin_history_request(channel_id)
        {
            return;
        }

        self.send_command(RuntimeCommand::LoadHistory {
            channel_id: channel_id.to_string(),
        });
    }

    fn force_next_channel_bottom_render(&self, channel_id: &str) {
        self.imp()
            .workspace_view
            .borrow_mut()
            .force_next_bottom(channel_id);
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
        imp.message_title
            .set_label(&self.conversation_title(channel_id));
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
        self.queue_history_render_followups(channel_id, messages);
    }

    fn queue_history_render_followups(&self, channel_id: &str, messages: Vec<SlackMessage>) {
        let weak_window = self.downgrade();
        let channel_id = channel_id.to_string();
        glib::idle_add_local_once(move || {
            if let Some(window) = weak_window.upgrade() {
                window.render_conversations();
                if window.visible_channel_id().as_deref() == Some(channel_id.as_str()) {
                    window.request_user_names(&messages);
                    window.request_image_assets(messages.iter());
                }
            }
        });
    }

    fn populate_thread(
        &self,
        channel_id: &str,
        ts: &str,
        messages: Vec<SlackMessage>,
        scroll_behavior: TimelineScrollBehavior,
    ) {
        let imp = self.imp();
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
        context.timeline_scroll = scroll_behavior;
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
        if let Some(position) = default_thread_pane_position(paned.width()) {
            paned.set_position(position);
        }
    }

    fn populate_activity(&self, items: Vec<ActivityItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Activity");
        self.render_conversations();
        self.load_message_html(&message_html::activity_document(&items));
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_label("Search results");
        let context = self.message_html_context(None);
        self.load_message_html(&message_html::search_results_document(&results, &context));
    }

    fn populate_files(&self, files: Vec<SlackFile>) {
        let imp = self.imp();
        imp.message_title.set_label("Files");
        self.render_conversations();
        self.load_message_html(&message_html::files_document(&files));
    }

    fn populate_saved_items(&self, items: Vec<SavedItem>) {
        let imp = self.imp();
        imp.message_title.set_label("Later");
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

    fn handle_socket_mode_event(&self, event: SocketModeEvent) {
        match event {
            SocketModeEvent::Message(event) => self.apply_socket_message(*event),
            SocketModeEvent::Reaction(event) => self.apply_socket_reaction(event),
            SocketModeEvent::RefreshConversations => self.refresh_conversations(),
        }
    }

    fn apply_socket_message(&self, event: SocketModeMessageEvent) {
        let channel_id = event.channel_id.clone();
        let message = event.message.clone();
        let reading_channel = self.visible_channel_id();
        let current_user_id = self.imp().current_user_id.borrow().clone();

        if realtime_message_marks_unread(
            reading_channel.as_deref(),
            current_user_id.as_deref(),
            &event,
        ) && !self.mark_conversation_locally_unread(&channel_id)
        {
            self.refresh_conversations();
        }

        let kind = match event.kind {
            SocketModeMessageKind::Posted => RealtimeMessageKind::Posted,
            SocketModeMessageKind::Changed => RealtimeMessageKind::Changed,
            SocketModeMessageKind::Deleted => RealtimeMessageKind::Deleted,
        };
        let outcome = self
            .imp()
            .workspace_view
            .borrow_mut()
            .apply_realtime_message(&channel_id, message.clone(), kind);

        if outcome.render_channel {
            let messages = self
                .imp()
                .workspace_view
                .borrow()
                .channel_messages(&channel_id)
                .to_vec();
            self.populate_history_with_scroll(
                &channel_id,
                messages,
                timeline_scroll_behavior(
                    outcome
                        .channel_scroll
                        .unwrap_or(WorkspaceScrollBehavior::Preserve),
                ),
            );
        }

        if outcome.render_thread {
            let snapshot = self.imp().workspace_view.borrow().snapshot();
            if let Some(thread_ts) = snapshot.thread_ts {
                self.populate_thread(
                    &channel_id,
                    &thread_ts,
                    snapshot.thread_messages,
                    timeline_scroll_behavior(if kind == RealtimeMessageKind::Posted {
                        WorkspaceScrollBehavior::StickToBottom
                    } else {
                        WorkspaceScrollBehavior::Preserve
                    }),
                );
            }
        }

        if event.kind == SocketModeMessageKind::Posted {
            self.notify_if_new_messages(&channel_id, std::slice::from_ref(&message));
        }
        self.request_user_names(std::slice::from_ref(&message));
        self.request_image_assets(std::iter::once(&message));

        if outcome.refresh_activity {
            self.populate_activity(self.activity_items());
        } else {
            self.render_conversations();
        }
    }

    fn apply_socket_reaction(&self, event: SocketModeReactionEvent) {
        let update = ReactionUpdate {
            channel_id: event.channel_id,
            ts: event.ts,
            name: event.name,
            user_id: event.user_id,
            added: event.added,
        };
        let outcome = self
            .imp()
            .workspace_view
            .borrow_mut()
            .apply_reaction(&update);

        if outcome.changed {
            self.rerender_current_messages();
        }
    }

    fn notify_if_new_messages(&self, channel_id: &str, messages: &[SlackMessage]) {
        let Some(latest_ts) = SlackMessage::latest_ts(messages.iter()) else {
            return;
        };

        let latest_message = messages.iter().find(|message| message.ts == latest_ts);
        let current_user_id = self.imp().current_user_id.borrow().clone();
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

        let selected_channel = self.visible_channel_id();
        let (has_unread, muted) = self.notification_conversation_state(channel_id);
        let action = message_notification_action(MessageNotificationState {
            channel_id,
            selected_channel: selected_channel.as_deref(),
            previous_latest_ts: previous_ts.as_deref(),
            latest_ts: latest_ts.as_str(),
            latest_message_user: latest_message.and_then(|message| message.user.as_deref()),
            current_user: current_user_id.as_deref(),
            has_unread,
            muted,
        });

        if action == MessageNotificationAction::Notify {
            self.send_notification(
                &self.conversation_title(channel_id),
                message_notification_body(latest_message),
            );
        }
    }

    fn notification_conversation_state(&self, channel_id: &str) -> (bool, bool) {
        self.imp()
            .conversations
            .borrow()
            .iter()
            .find(|conversation| conversation.id == channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    conversation.is_muted_conversation(),
                )
            })
            .unwrap_or((false, false))
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
        let identity = self.imp().request_coordinator.borrow_mut().issue(&command);
        self.send_identified_command(identity, command);
    }

    fn send_session_command(&self, command: RuntimeCommand) {
        let identity = self
            .imp()
            .request_coordinator
            .borrow_mut()
            .begin_session(&command);
        self.send_identified_command(identity, command);
    }

    fn send_identified_command(&self, identity: RuntimeIdentity, command: RuntimeCommand) {
        let runtime = self.imp().runtime.borrow().clone();
        if let Some(runtime) = runtime {
            runtime.send(identity, command);
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
            .flat_map(SlackConversation::display_user_ids)
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
                    self.populate_thread(
                        &channel_id,
                        &thread_ts,
                        snapshot.thread_messages,
                        TimelineScrollBehavior::Preserve,
                    );
                }
            }
        }
    }

    fn message_html_context(&self, thread_ts: Option<&str>) -> MessageHtmlContext {
        let imp = self.imp();
        MessageHtmlContext {
            user_names: imp.user_names.borrow().clone(),
            user_group_names: imp.user_group_names.borrow().clone(),
            user_group_members: imp.user_group_members.borrow().clone(),
            current_user_id: imp.current_user_id.borrow().clone(),
            thread_ts: thread_ts.map(ToString::to_string),
            load_more_url: None,
            timeline_scroll: TimelineScrollBehavior::Preserve,
            image_assets: imp.image_assets.borrow().clone(),
            failed_image_urls: imp.failed_image_assets.borrow().clone(),
        }
    }

    fn current_message_snapshot(&self) -> WorkspaceSnapshot {
        self.imp().workspace_view.borrow().snapshot()
    }

    fn selected_channel_id(&self) -> Option<String> {
        self.imp()
            .workspace_view
            .borrow()
            .last_channel_id()
            .map(ToString::to_string)
    }

    fn visible_channel_id(&self) -> Option<String> {
        self.imp()
            .workspace_view
            .borrow()
            .visible_channel_id()
            .map(ToString::to_string)
    }

    fn selected_thread_ts(&self) -> Option<String> {
        self.imp()
            .workspace_view
            .borrow()
            .selected_thread_ts()
            .map(ToString::to_string)
    }

    fn current_main_view(&self) -> MainMessageView {
        self.imp().workspace_view.borrow().main_view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimeOperation, RuntimeTarget};
    use crate::sidebar::ConversationKind;

    fn sidebar_row(id: &str, title: &str) -> SidebarRowModel {
        SidebarRowModel {
            id: id.to_string(),
            title: title.to_string(),
            kind: ConversationKind::DirectMessage,
            unread: false,
            unread_count: 0,
            selected: false,
            private: true,
            muted: false,
            external: false,
        }
    }

    #[test]
    fn request_coordinator_rejects_superseded_and_previous_session_responses() {
        let mut coordinator = RequestCoordinator::default();
        let first = coordinator.begin_session(&RuntimeCommand::SearchMessages {
            query: "first".to_string(),
        });
        let second = coordinator.issue(&RuntimeCommand::SearchMessages {
            query: "second".to_string(),
        });
        let context = OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace);

        assert!(!coordinator.accepts(&RuntimeEventMeta::new(first, context.clone())));
        assert!(coordinator.accepts(&RuntimeEventMeta::new(second, context.clone())));

        let signed_out = coordinator.begin_session(&RuntimeCommand::SignOut);
        assert!(!coordinator.accepts(&RuntimeEventMeta::new(second, context)));
        assert!(!coordinator.accepts(&RuntimeEventMeta {
            session: second.session,
            request: None,
            context: OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace,),
        }));
        assert!(coordinator.accepts(&RuntimeEventMeta::new(
            signed_out,
            OperationContext::new(RuntimeOperation::SignOut, RuntimeTarget::Workspace),
        )));
    }

    #[test]
    fn request_coordinator_accepts_cached_and_fresh_events_for_current_request() {
        let mut coordinator = RequestCoordinator::default();
        let identity = coordinator.begin_session(&RuntimeCommand::LoadHistory {
            channel_id: "C123".to_string(),
        });
        let context = OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C123".to_string()),
        );

        let cached = RuntimeEventMeta::new(identity, context.clone());
        let fresh = RuntimeEventMeta::new(identity, context);

        assert!(coordinator.accepts(&cached));
        assert!(coordinator.accepts(&fresh));
    }

    #[test]
    fn request_coordinator_accepts_all_mutation_completions() {
        let mut coordinator = RequestCoordinator::default();
        let first = coordinator.begin_session(&RuntimeCommand::SetSaved {
            channel_id: "C123".to_string(),
            ts: "1.0".to_string(),
            add: true,
            thread_ts: None,
        });
        let second = coordinator.issue(&RuntimeCommand::SetSaved {
            channel_id: "C123".to_string(),
            ts: "2.0".to_string(),
            add: true,
            thread_ts: None,
        });
        let context = OperationContext::new(
            RuntimeOperation::Saved,
            RuntimeTarget::Message {
                channel_id: "C123".to_string(),
                thread_ts: None,
            },
        );

        assert!(coordinator.accepts(&RuntimeEventMeta::new(first, context.clone())));
        assert!(coordinator.accepts(&RuntimeEventMeta::new(second, context)));
    }

    #[test]
    fn startup_runtime_error_is_terminal_for_event_delivery() {
        let event = RuntimeEvent {
            meta: RuntimeEventMeta::new(
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
            ),
            kind: RuntimeEventKind::RuntimeStartFailed("runtime construction failed".to_string()),
        };

        assert!(runtime_event_is_start_failure(&event));

        let ordinary_error = RuntimeEvent {
            meta: RuntimeEventMeta::new(
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(2),
                },
                OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
            ),
            kind: RuntimeEventKind::Error("stored token failed".to_string()),
        };
        assert!(!runtime_event_is_start_failure(&ordinary_error));
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
    fn sidebar_error_change_preserves_populated_list() {
        assert!(sidebar_error_change_needs_render(false));
        assert!(!sidebar_error_change_needs_render(true));
    }

    #[test]
    fn connected_workspace_status_uses_workspace_name_when_available() {
        assert_eq!(
            connected_workspace_status(Some("Signicat")),
            "Connected to Signicat"
        );
        assert_eq!(connected_workspace_status(None), "Connected to Slack");
    }

    #[test]
    fn sidebar_user_name_updates_render_for_idle_dm_and_group_dm_rows() {
        let dm = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let group_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "is_mpim": true,
            "members": ["U456", "U789"]
        }))
        .expect("failed to parse group direct message");
        let channel = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let conversations = vec![dm, group_dm, channel];

        assert!(sidebar_user_name_update_needs_render(
            &conversations,
            "U123",
            false
        ));
        assert!(sidebar_user_name_update_needs_render(
            &conversations,
            "U456",
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
    fn conversation_refresh_start_keeps_sidebar_visually_backgrounded() {
        assert!(!conversation_refresh_start_shows_sidebar_loading());
    }

    #[test]
    fn message_web_view_features_enable_internal_scroll_runtime() {
        let features = message_web_view_feature_policy();

        assert!(features.javascript);
        assert!(features.html5_local_storage);
        assert!(!features.file_url_access);
        assert!(!features.universal_file_url_access);
        assert!(!features.media);
        assert!(!features.webgl);
        assert!(!features.webaudio);
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
    fn notification_policy_notifies_only_for_new_unread_incoming_messages() {
        assert_eq!(
            message_notification_action(MessageNotificationState {
                channel_id: "C123",
                selected_channel: Some("C456"),
                previous_latest_ts: Some("1710000100.000000"),
                latest_ts: "1710000200.000000",
                latest_message_user: Some("U456"),
                current_user: Some("U123"),
                has_unread: true,
                muted: false,
            }),
            MessageNotificationAction::Notify
        );
    }

    #[test]
    fn notification_policy_records_without_notifying_for_non_notifyable_messages() {
        let notifyable = MessageNotificationState {
            channel_id: "C123",
            selected_channel: Some("C456"),
            previous_latest_ts: Some("1710000100.000000"),
            latest_ts: "1710000200.000000",
            latest_message_user: Some("U456"),
            current_user: Some("U123"),
            has_unread: true,
            muted: false,
        };

        assert_eq!(
            message_notification_action(MessageNotificationState {
                previous_latest_ts: None,
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
        assert_eq!(
            message_notification_action(MessageNotificationState {
                selected_channel: Some("C123"),
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
        assert_eq!(
            message_notification_action(MessageNotificationState {
                has_unread: false,
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
        assert_eq!(
            message_notification_action(MessageNotificationState {
                muted: true,
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
        assert_eq!(
            message_notification_action(MessageNotificationState {
                latest_message_user: Some("U123"),
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
    }

    #[test]
    fn notification_body_uses_fallback_for_empty_message_text() {
        assert_eq!(message_notification_body(None), "New message");
        assert_eq!(
            message_notification_body(Some(&SlackMessage {
                ts: "1710000100.000000".to_string(),
                text: Some("   ".to_string()),
                ..Default::default()
            })),
            "New message"
        );
        assert_eq!(
            message_notification_body(Some(&message("1710000200.000000", "Hello"))),
            "Hello"
        );
    }

    #[test]
    fn default_thread_pane_position_uses_readable_thread_width() {
        assert_eq!(default_thread_pane_position(0), None);
        assert_eq!(default_thread_pane_position(600), Some(300));
        assert_eq!(default_thread_pane_position(820), Some(440));
        assert_eq!(default_thread_pane_position(1060), Some(636));
        assert_eq!(default_thread_pane_position(1400), Some(900));
    }

    #[test]
    fn realtime_messages_mark_unread_only_when_not_reading_or_self_sent() {
        let event = SocketModeMessageEvent {
            channel_id: "C123".to_string(),
            kind: SocketModeMessageKind::Posted,
            message: SlackMessage {
                user: Some("U123".to_string()),
                ts: "1710000300.000000".to_string(),
                ..Default::default()
            },
        };
        let changed = SocketModeMessageEvent {
            kind: SocketModeMessageKind::Changed,
            ..event.clone()
        };

        assert!(!realtime_message_marks_unread(
            Some("C123"),
            Some("U999"),
            &event
        ));
        assert!(!realtime_message_marks_unread(None, Some("U123"), &event));
        assert!(!realtime_message_marks_unread(None, Some("U999"), &changed));
        assert!(realtime_message_marks_unread(
            Some("C999"),
            Some("U999"),
            &event
        ));
    }

    #[test]
    fn mutation_completion_reloads_only_the_visible_channel() {
        assert!(mutation_completion_reloads_visible_channel(
            Some("C123"),
            "C123"
        ));
        assert!(!mutation_completion_reloads_visible_channel(
            Some("C456"),
            "C123"
        ));
        assert!(!mutation_completion_reloads_visible_channel(None, "C123"));
    }
}
