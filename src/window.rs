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
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::activity::{self, ActivityItem};
use crate::auth;
use crate::composer::{
    set_text_view_text, text_view_enter_action, text_view_text, TextViewEnterAction,
};
use crate::config;
use crate::drafts::{DraftKey, DraftSettings, Drafts};
use crate::message_html::{self, MessageHtmlContext, TimelineScrollBehavior};
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation, SlackFile,
    SlackMessage, SlackUnreadState, SlackUser,
};
use crate::rendering;
use crate::runtime::{
    AppRuntime, OperationContext, RequestId, RuntimeCommand, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimeIdentity, RuntimeOperation, RuntimeTarget, SessionId,
};
use crate::shortcuts::WINDOW_SHORTCUTS;
use crate::sidebar::{
    self, ConversationPickerAction, ConversationPickerItem, ConversationPickerSections,
    SidebarRowModel, SidebarSectionModel,
};
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
        pub client_id_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub browser_session_check: TemplateChild<gtk::CheckButton>,
        #[template_child]
        pub xoxc_token_entry: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub xoxd_token_entry: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub user_agent_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub setup_hint_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub connect_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub connection_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub workspace_title_label: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub workspace_split: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub messages_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub unreads_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub files_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub saved_button: TemplateChild<gtk::ToggleButton>,
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
        pub message_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub message_view_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub message_composer: TemplateChild<gtk::Box>,
        #[template_child]
        pub thread_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub message_entry: TemplateChild<gtk::TextView>,
        #[template_child]
        pub send_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upload_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub upload_progress: TemplateChild<gtk::ProgressBar>,
        #[template_child]
        pub message_search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child]
        pub message_search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub message_search_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub thread_title: TemplateChild<adw::WindowTitle>,
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
        pub discovered_channels: RefCell<Vec<SlackConversation>>,
        pub discovered_users: RefCell<Vec<SlackUser>>,
        pub(super) sidebar_row_actions: RefCell<HashMap<i32, SidebarRowAction>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub user_group_names: RefCell<HashMap<String, String>>,
        pub user_group_members: RefCell<HashMap<String, Vec<String>>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub workspace_id: RefCell<Option<String>>,
        pub workspace_name: RefCell<Option<String>>,
        pub workspace_url: RefCell<Option<String>>,
        pub workspace_ready: Cell<bool>,
        pub(super) pending_notification_target: RefCell<Option<NotificationTarget>>,
        pub drafts: RefCell<Drafts>,
        pub draft_save_generation: Cell<u64>,
        pub pending_sent_drafts: RefCell<HashMap<DraftKey, String>>,
        pub pending_upload_drafts: RefCell<HashMap<String, Option<String>>>,
        pub sidebar_loading: Cell<bool>,
        pub sidebar_error: RefCell<Option<String>>,
        pub(super) workspace_view: RefCell<WorkspaceViewState>,
        pub current_user_id: RefCell<Option<String>>,
        pub message_view: RefCell<Option<webkit6::WebView>>,
        pub(super) media_viewer: RefCell<Option<MediaViewer>>,
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
            obj.setup_adaptive_layout();
            obj.setup_runtime();
            obj.setup_message_view();
            obj.configure_accessibility();
            obj.configure_auth_ui();
            obj.setup_settings();
            obj.setup_callbacks();
            if std::env::var_os("CONDUIT_TEST_WORKSPACE").is_some() {
                obj.show_workspace(AuthInfo {
                    team: Some("Test Workspace".to_string()),
                    ..AuthInfo::default()
                });
                obj.populate_conversations(vec![SlackConversation {
                    id: "C_TEST".to_string(),
                    name: Some("general".to_string()),
                    is_channel: Some(true),
                    ..SlackConversation::default()
                }]);
            } else {
                obj.show_loading("Checking secure storage");
                obj.send_session_command(RuntimeCommand::LoadStoredToken);
            }
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
    action: ConversationPickerAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MediaGalleryItem {
    url: String,
    name: String,
    kind: MediaKind,
}

#[derive(Debug)]
struct MediaViewer {
    surface_stack: gtk::Stack,
    content_stack: gtk::Stack,
    image_scroller: gtk::ScrolledWindow,
    picture: gtk::Picture,
    title: gtk::Label,
    zoom_label: gtk::Label,
    zoom_out_button: gtk::Button,
    zoom_in_button: gtk::Button,
    zoom_reset_button: gtk::Button,
    previous_button: gtk::Button,
    next_button: gtk::Button,
    gallery: Vec<MediaGalleryItem>,
    index: usize,
    zoom: f64,
    natural_size: (i32, i32),
    loaded_path: Option<PathBuf>,
}

impl SidebarRowAction {
    fn from_model(model: &SidebarRowModel) -> Self {
        Self {
            channel_id: model.id.clone(),
            title: model.title.clone(),
            action: ConversationPickerAction::OpenConversation,
        }
    }

    fn from_picker_item(item: &ConversationPickerItem) -> Self {
        Self {
            channel_id: item.row.id.clone(),
            title: item.row.title.clone(),
            action: item.action,
        }
    }
}

fn sidebar_row_action_for_index(
    actions: &HashMap<i32, SidebarRowAction>,
    row_index: i32,
) -> Option<SidebarRowAction> {
    actions.get(&row_index).cloned()
}

fn picker_sections(
    include_discovery: bool,
    conversations: &[SlackConversation],
    discovered_channels: &[SlackConversation],
    discovered_users: &[SlackUser],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
) -> ConversationPickerSections {
    let channels = if include_discovery {
        discovered_channels
    } else {
        &[]
    };
    let users = if include_discovery {
        discovered_users
    } else {
        &[]
    };
    sidebar::conversation_picker_sections(
        conversations,
        channels,
        users,
        user_names,
        current_user_id,
        query,
    )
}

fn picker_sections_empty(sections: &ConversationPickerSections) -> bool {
    sections.conversations.is_empty() && sections.channels.is_empty() && sections.people.is_empty()
}

fn media_gallery_items(messages: &[SlackMessage]) -> Vec<MediaGalleryItem> {
    messages
        .iter()
        .flat_map(|message| message.files.as_deref().unwrap_or_default())
        .filter_map(|file| {
            let kind = match file.supported_media_kind()? {
                "image" => MediaKind::Image,
                "video" => MediaKind::Video,
                _ => return None,
            };
            Some(MediaGalleryItem {
                url: file.media_url()?.to_string(),
                name: file.display_title().to_string(),
                kind,
            })
        })
        .collect()
}

fn apply_media_zoom(viewer: &MediaViewer) {
    viewer
        .zoom_label
        .set_label(&format!("{:.0}%", viewer.zoom * 100.0));
    let viewport_width = viewer.image_scroller.width().max(1);
    let viewport_height = viewer.image_scroller.height().max(1);
    let (width, height) = media_zoom_size(
        viewer.natural_size,
        (viewport_width, viewport_height),
        viewer.zoom,
    );
    viewer.picture.set_size_request(width, height);
}

fn media_zoom_size(natural: (i32, i32), viewport: (i32, i32), zoom: f64) -> (i32, i32) {
    let natural_width = natural.0.max(1) as f64;
    let natural_height = natural.1.max(1) as f64;
    let fit_scale = (viewport.0.max(1) as f64 / natural_width)
        .min(viewport.1.max(1) as f64 / natural_height)
        .min(1.0);
    (
        (natural_width * fit_scale * zoom).round().max(1.0) as i32,
        (natural_height * fit_scale * zoom).round().max(1.0) as i32,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceNavigationSelection {
    Messages,
    Unreads,
    Files,
    Saved,
}

fn workspace_navigation_selection(
    main_view: MainMessageView,
) -> Option<WorkspaceNavigationSelection> {
    match main_view {
        MainMessageView::Conversation => Some(WorkspaceNavigationSelection::Messages),
        MainMessageView::Unreads => Some(WorkspaceNavigationSelection::Unreads),
        MainMessageView::Files => Some(WorkspaceNavigationSelection::Files),
        MainMessageView::Saved => Some(WorkspaceNavigationSelection::Saved),
        MainMessageView::Placeholder | MainMessageView::Search => None,
    }
}

fn workspace_composer_visible(main_view: MainMessageView) -> bool {
    main_view == MainMessageView::Conversation
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

fn message_notification_body(message: Option<&SlackMessage>) -> String {
    message
        .and_then(|message| message.text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| gettext("New message"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationTarget {
    workspace_id: String,
    channel_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationTargetResolution {
    Wait,
    Open,
    RejectWorkspace,
}

fn notification_target_resolution(
    current_workspace_id: Option<&str>,
    workspace_ready: bool,
    target: &NotificationTarget,
) -> NotificationTargetResolution {
    match current_workspace_id {
        None => NotificationTargetResolution::Wait,
        Some(workspace_id) if workspace_id != target.workspace_id => {
            NotificationTargetResolution::RejectWorkspace
        }
        Some(_) if !workspace_ready => NotificationTargetResolution::Wait,
        Some(_) => NotificationTargetResolution::Open,
    }
}

fn workspace_identity(auth: &AuthInfo) -> Option<String> {
    let workspace = [
        auth.team_id.as_deref(),
        auth.url.as_deref(),
        auth.team.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())?;
    let user = [auth.user_id.as_deref(), auth.user.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty());
    Some(user.map_or_else(
        || workspace.to_string(),
        |user| format!("{workspace}:{user}"),
    ))
}

fn submitted_draft_matches(
    current_text: Option<&str>,
    stored_text: Option<&str>,
    submitted: &str,
) -> bool {
    current_text
        .or(stored_text)
        .is_some_and(|text| text.trim() == submitted)
}

fn posted_message_thread_ts(
    context: &OperationContext,
    channel_id: &str,
    message: &SlackMessage,
) -> Option<String> {
    match &context.target {
        RuntimeTarget::Message {
            channel_id: target_channel_id,
            thread_ts,
        } if target_channel_id == channel_id => thread_ts.clone(),
        _ => message.thread_ts.clone(),
    }
}

fn record_draft_submission(
    pending: &mut HashMap<DraftKey, String>,
    key: DraftKey,
    text: &str,
) -> bool {
    if pending.contains_key(&key) {
        return false;
    }
    pending.insert(key, text.to_string());
    true
}

fn record_upload_submission(
    pending: &mut HashMap<String, Option<String>>,
    channel_id: &str,
    initial_comment: Option<String>,
) -> bool {
    if pending.contains_key(channel_id) {
        return false;
    }
    pending.insert(channel_id.to_string(), initial_comment);
    true
}

fn conversation_refresh_start_shows_sidebar_loading() -> bool {
    false
}

fn sidebar_error_change_needs_render(has_conversations: bool) -> bool {
    !has_conversations
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeFailureRecovery {
    Session,
    Sidebar,
    History(String),
    Thread {
        channel_id: String,
        thread_ts: String,
    },
    Search,
    Files,
    SavedItems,
    User(String),
    Image(String),
    Media,
    PostMessage {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Reaction {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Saved {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Upload(String),
    NonDisruptive,
}

fn runtime_failure_recovery(context: &OperationContext) -> RuntimeFailureRecovery {
    match (&context.operation, &context.target) {
        (
            RuntimeOperation::Startup
            | RuntimeOperation::Authenticate
            | RuntimeOperation::SignOut
            | RuntimeOperation::Disconnect,
            RuntimeTarget::Workspace,
        ) => RuntimeFailureRecovery::Session,
        (RuntimeOperation::Conversations, RuntimeTarget::Workspace) => {
            RuntimeFailureRecovery::Sidebar
        }
        (
            RuntimeOperation::History | RuntimeOperation::OlderHistory,
            RuntimeTarget::Channel(channel_id),
        ) => RuntimeFailureRecovery::History(channel_id.clone()),
        (
            RuntimeOperation::Thread | RuntimeOperation::OlderThread,
            RuntimeTarget::Thread {
                channel_id,
                thread_ts,
            },
        ) => RuntimeFailureRecovery::Thread {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        },
        (RuntimeOperation::Search, RuntimeTarget::Workspace) => RuntimeFailureRecovery::Search,
        (RuntimeOperation::Files, RuntimeTarget::Workspace) => RuntimeFailureRecovery::Files,
        (RuntimeOperation::SavedItems, RuntimeTarget::Workspace) => {
            RuntimeFailureRecovery::SavedItems
        }
        (RuntimeOperation::User, RuntimeTarget::User(user_id)) => {
            RuntimeFailureRecovery::User(user_id.clone())
        }
        (RuntimeOperation::ImageAsset, RuntimeTarget::Image(key)) => {
            RuntimeFailureRecovery::Image(key.clone())
        }
        (RuntimeOperation::Media, RuntimeTarget::Media(_)) => RuntimeFailureRecovery::Media,
        (
            RuntimeOperation::PostMessage,
            RuntimeTarget::Message {
                channel_id,
                thread_ts,
            },
        ) => RuntimeFailureRecovery::PostMessage {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        },
        (
            RuntimeOperation::Reaction,
            RuntimeTarget::Message {
                channel_id,
                thread_ts,
            },
        ) => RuntimeFailureRecovery::Reaction {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        },
        (
            RuntimeOperation::Saved,
            RuntimeTarget::Message {
                channel_id,
                thread_ts,
            },
        ) => RuntimeFailureRecovery::Saved {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        },
        (RuntimeOperation::FileUpload, RuntimeTarget::Upload(channel_id)) => {
            RuntimeFailureRecovery::Upload(channel_id.clone())
        }
        _ => RuntimeFailureRecovery::NonDisruptive,
    }
}

fn mutation_target_is_active(
    visible_channel: Option<&str>,
    selected_thread: Option<&str>,
    target_channel: &str,
    target_thread: Option<&str>,
) -> bool {
    visible_channel == Some(target_channel)
        && target_thread.is_none_or(|thread_ts| selected_thread == Some(thread_ts))
}

fn connected_workspace_status(workspace_name: Option<&str>) -> String {
    format!("Connected to {}", workspace_name.unwrap_or("Slack"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderSurface {
    Messages,
    SearchResults,
    Files,
    SavedItems,
}

impl PlaceholderSurface {
    fn title(self) -> String {
        match self {
            Self::Messages => gettext("Messages"),
            Self::SearchResults => gettext("Search results"),
            Self::Files => gettext("Files"),
            Self::SavedItems => gettext("Later"),
        }
    }

    fn error_message(self, error: &str) -> String {
        let template = match self {
            Self::Messages => gettext("Could not load messages. Try again. {error}"),
            Self::SearchResults => gettext("Could not load search results. Try again. {error}"),
            Self::Files => gettext("Could not load files. Try again. {error}"),
            Self::SavedItems => gettext("Could not load saved items. Try again. {error}"),
        };
        template.replace("{error}", error)
    }
}

fn localized_replies_error(error: &str) -> String {
    gettext("Could not load replies. Try again. {error}").replace("{error}", error)
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

fn promoted_recent_reactions<'a>(
    names: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Vec<String> {
    let mut promoted = Vec::with_capacity(3);
    if !name.trim().is_empty() {
        promoted.push(name.to_string());
    }
    for existing in names {
        if promoted.len() == 3 {
            break;
        }
        if !existing.trim().is_empty() && !promoted.iter().any(|value| value == existing) {
            promoted.push(existing.to_string());
        }
    }
    promoted
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

    fn setup_adaptive_layout(&self) {
        let imp = self.imp();

        let workspace_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            700.0,
            adw::LengthUnit::Sp,
        ));
        workspace_breakpoint.add_setter(
            &imp.workspace_split.get(),
            "collapsed",
            Some(&true.to_value()),
        );
        self.add_breakpoint(workspace_breakpoint);

        let thread_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            900.0,
            adw::LengthUnit::Sp,
        ));
        thread_breakpoint.add_setter(&imp.thread_split.get(), "collapsed", Some(&true.to_value()));
        thread_breakpoint.add_setter(
            &imp.thread_split.get(),
            "pin-sidebar",
            Some(&false.to_value()),
        );
        thread_breakpoint.add_setter(
            &imp.thread_split.get(),
            "sidebar-width-fraction",
            Some(&1.0_f64.to_value()),
        );
        thread_breakpoint.add_setter(
            &imp.thread_split.get(),
            "max-sidebar-width",
            Some(&1000.0_f64.to_value()),
        );
        self.add_breakpoint(thread_breakpoint);
    }

    fn configure_accessibility(&self) {
        let imp = self.imp();
        imp.message_entry
            .update_property(&[gtk::accessible::Property::Label("Message")]);
        imp.thread_entry
            .update_property(&[gtk::accessible::Property::Label("Reply")]);
        imp.message_search_entry
            .update_property(&[gtk::accessible::Property::Label(
                "Search workspace messages",
            )]);

        for (button, label) in [
            (
                imp.messages_button.get().upcast::<gtk::Widget>(),
                gettext("Messages"),
            ),
            (
                imp.unreads_button.get().upcast::<gtk::Widget>(),
                gettext("Unreads"),
            ),
            (
                imp.files_button.get().upcast::<gtk::Widget>(),
                gettext("Files"),
            ),
            (
                imp.saved_button.get().upcast::<gtk::Widget>(),
                gettext("Later"),
            ),
        ] {
            button.update_property(&[gtk::accessible::Property::Label(&label)]);
        }
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
                window.show_session_error("Background runtime stopped");
            }
        });
    }

    fn setup_message_view(&self) {
        let network_session = self.create_message_network_session();

        let message_view = self.create_message_web_view(&network_session);
        let viewer = self.create_media_viewer(&message_view);
        self.imp().message_view_box.append(&viewer.surface_stack);
        *self.imp().message_view.borrow_mut() = Some(message_view);
        *self.imp().media_viewer.borrow_mut() = Some(viewer);
        self.setup_media_viewer_callbacks();

        let thread_view = self.create_message_web_view(&network_session);
        self.imp().thread_view_box.append(&thread_view);
        *self.imp().thread_view.borrow_mut() = Some(thread_view);

        self.show_message_placeholder(&gettext("Select a conversation"));
        self.load_thread_html(&message_html::placeholder_document(
            &gettext("Thread"),
            &gettext("No thread open"),
        ));
    }

    fn create_media_viewer(&self, message_view: &webkit6::WebView) -> MediaViewer {
        let surface_stack = gtk::Stack::new();
        surface_stack.set_hexpand(true);
        surface_stack.set_vexpand(true);
        surface_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        surface_stack.add_named(message_view, Some("timeline"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("view");
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.set_margin_top(6);
        toolbar.set_margin_bottom(6);
        toolbar.set_margin_start(6);
        toolbar.set_margin_end(6);

        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_tooltip_text(Some("Close media viewer"));
        let previous_button = gtk::Button::from_icon_name("go-previous-symbolic");
        previous_button.set_tooltip_text(Some("Previous media"));
        let next_button = gtk::Button::from_icon_name("go-next-symbolic");
        next_button.set_tooltip_text(Some("Next media"));
        let title = gtk::Label::new(None);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        title.set_xalign(0.0);

        let zoom_out = gtk::Button::from_icon_name("zoom-out-symbolic");
        zoom_out.set_tooltip_text(Some("Zoom out"));
        let zoom_label = gtk::Label::new(Some("100%"));
        zoom_label.set_width_chars(5);
        let zoom_in = gtk::Button::from_icon_name("zoom-in-symbolic");
        zoom_in.set_tooltip_text(Some("Zoom in"));
        let zoom_reset = gtk::Button::from_icon_name("zoom-original-symbolic");
        zoom_reset.set_tooltip_text(Some("Reset zoom"));
        let save = gtk::Button::from_icon_name("document-save-symbolic");
        save.set_tooltip_text(Some("Save media as"));
        let fullscreen = gtk::Button::from_icon_name("view-fullscreen-symbolic");
        fullscreen.set_tooltip_text(Some("Toggle fullscreen"));

        for widget in [
            close.upcast_ref::<gtk::Widget>(),
            previous_button.upcast_ref(),
            next_button.upcast_ref(),
            title.upcast_ref(),
            zoom_out.upcast_ref(),
            zoom_label.upcast_ref(),
            zoom_in.upcast_ref(),
            zoom_reset.upcast_ref(),
            save.upcast_ref(),
            fullscreen.upcast_ref(),
        ] {
            toolbar.append(widget);
        }
        root.append(&toolbar);

        let content_stack = gtk::Stack::new();
        content_stack.set_hexpand(true);
        content_stack.set_vexpand(true);
        content_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        let image_scroller = gtk::ScrolledWindow::new();
        image_scroller.set_hexpand(true);
        image_scroller.set_vexpand(true);
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_halign(gtk::Align::Center);
        picture.set_valign(gtk::Align::Center);
        let image_canvas = gtk::CenterBox::new();
        image_canvas.set_orientation(gtk::Orientation::Vertical);
        image_canvas.set_hexpand(true);
        image_canvas.set_vexpand(true);
        image_canvas.set_center_widget(Some(&picture));
        image_scroller.set_child(Some(&image_canvas));
        content_stack.add_named(&image_scroller, Some("image"));

        let loading = gtk::Spinner::new();
        loading.set_spinning(true);
        loading.set_halign(gtk::Align::Center);
        loading.set_valign(gtk::Align::Center);
        content_stack.add_named(&loading, Some("loading"));
        root.append(&content_stack);
        surface_stack.add_named(&root, Some("media"));
        surface_stack.set_visible_child_name("timeline");

        self.connect_media_viewer_button(&close, |window| window.close_media_viewer());
        self.connect_media_viewer_button(&previous_button, |window| window.navigate_media(-1));
        self.connect_media_viewer_button(&next_button, |window| window.navigate_media(1));
        self.connect_media_viewer_button(&zoom_out, |window| window.adjust_media_zoom(0.8));
        self.connect_media_viewer_button(&zoom_in, |window| window.adjust_media_zoom(1.25));
        self.connect_media_viewer_button(&zoom_reset, |window| window.reset_media_zoom());
        self.connect_media_viewer_button(&save, |window| window.save_current_media());
        self.connect_media_viewer_button(&fullscreen, |window| window.toggle_media_fullscreen());

        MediaViewer {
            surface_stack,
            content_stack,
            image_scroller,
            picture,
            title,
            zoom_label,
            zoom_out_button: zoom_out,
            zoom_in_button: zoom_in,
            zoom_reset_button: zoom_reset,
            previous_button,
            next_button,
            gallery: Vec::new(),
            index: 0,
            zoom: 1.0,
            natural_size: (0, 0),
            loaded_path: None,
        }
    }

    fn connect_media_viewer_button<F>(&self, button: &gtk::Button, callback: F)
    where
        F: Fn(&Self) + 'static,
    {
        let weak_window = self.downgrade();
        button.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                callback(&window);
            }
        });
    }

    fn setup_media_viewer_callbacks(&self) {
        let viewer_ref = self.imp().media_viewer.borrow();
        let Some(viewer) = viewer_ref.as_ref() else {
            return;
        };

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let weak_window = self.downgrade();
        scroll.connect_scroll(move |_, _, dy| {
            if let Some(window) = weak_window.upgrade() {
                window.adjust_media_zoom(if dy < 0.0 { 1.1 } else { 1.0 / 1.1 });
            }
            glib::Propagation::Stop
        });
        viewer.image_scroller.add_controller(scroll);

        let close_click = gtk::GestureClick::new();
        close_click.set_button(gtk::gdk::BUTTON_PRIMARY);
        let weak_window = self.downgrade();
        close_click.connect_released(move |_, _, _, _| {
            if let Some(window) = weak_window.upgrade() {
                window.close_media_viewer();
            }
        });
        viewer.picture.add_controller(close_click);

        let context_click = gtk::GestureClick::new();
        context_click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let weak_window = self.downgrade();
        context_click.connect_pressed(move |_, _, x, y| {
            if let Some(window) = weak_window.upgrade() {
                window.show_media_context_menu(x, y);
            }
        });
        viewer.content_stack.add_controller(context_click);

        let swipe = gtk::GestureSwipe::new();
        let weak_window = self.downgrade();
        swipe.connect_swipe(move |_, velocity_x, _| {
            if velocity_x.abs() >= 100.0 {
                if let Some(window) = weak_window.upgrade() {
                    window.navigate_media(if velocity_x < 0.0 { 1 } else { -1 });
                }
            }
        });
        viewer.content_stack.add_controller(swipe);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_window = self.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(window) = weak_window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gtk::gdk::Key::Escape => window.close_media_viewer(),
                gtk::gdk::Key::Left => window.navigate_media(-1),
                gtk::gdk::Key::Right => window.navigate_media(1),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        viewer.surface_stack.add_controller(keys);
    }

    fn open_media_viewer(&self, item: MediaGalleryItem) {
        let snapshot = self.current_message_snapshot();
        let mut gallery = media_gallery_items(&snapshot.channel_messages);
        if !gallery.iter().any(|candidate| candidate.url == item.url) {
            gallery.push(item.clone());
        }
        let index = gallery
            .iter()
            .position(|candidate| candidate.url == item.url)
            .unwrap_or_default();
        if let Some(viewer) = self.imp().media_viewer.borrow_mut().as_mut() {
            viewer.gallery = gallery;
            viewer.index = index;
            viewer.surface_stack.set_visible_child_name("media");
        }
        self.imp().message_composer.set_visible(false);
        self.imp().message_status_label.set_visible(false);
        self.load_current_media();
    }

    fn load_current_media(&self) {
        let item = {
            let mut viewer_ref = self.imp().media_viewer.borrow_mut();
            let Some(viewer) = viewer_ref.as_mut() else {
                return;
            };
            let Some(item) = viewer.gallery.get(viewer.index).cloned() else {
                return;
            };
            viewer.title.set_label(&item.name);
            viewer.loaded_path = None;
            viewer.content_stack.set_visible_child_name("loading");
            for button in [
                &viewer.zoom_out_button,
                &viewer.zoom_in_button,
                &viewer.zoom_reset_button,
            ] {
                button.set_sensitive(false);
            }
            viewer.previous_button.set_sensitive(viewer.index > 0);
            viewer
                .next_button
                .set_sensitive(viewer.index + 1 < viewer.gallery.len());
            item
        };
        self.reset_media_zoom();
        self.set_status("Loading media");
        self.send_command(RuntimeCommand::LoadMedia {
            url: item.url,
            name: item.name,
        });
    }

    fn navigate_media(&self, offset: i32) {
        let changed = {
            let mut viewer_ref = self.imp().media_viewer.borrow_mut();
            let Some(viewer) = viewer_ref.as_mut() else {
                return;
            };
            let next = viewer.index as i32 + offset;
            if next < 0 || next >= viewer.gallery.len() as i32 {
                false
            } else {
                viewer.index = next as usize;
                true
            }
        };
        if changed {
            self.load_current_media();
        }
    }

    fn adjust_media_zoom(&self, factor: f64) {
        if let Some(viewer) = self.imp().media_viewer.borrow_mut().as_mut() {
            if viewer.content_stack.visible_child_name().as_deref() != Some("image") {
                return;
            }
            viewer.zoom = (viewer.zoom * factor).clamp(0.1, 8.0);
            apply_media_zoom(viewer);
        }
    }

    fn reset_media_zoom(&self) {
        if let Some(viewer) = self.imp().media_viewer.borrow_mut().as_mut() {
            viewer.zoom = 1.0;
            apply_media_zoom(viewer);
        }
    }

    fn close_media_viewer(&self) {
        if let Some(viewer) = self.imp().media_viewer.borrow().as_ref() {
            viewer.surface_stack.set_visible_child_name("timeline");
        }
        self.imp().message_status_label.set_visible(true);
        self.sync_workspace_chrome();
        if self.is_fullscreen() {
            self.unfullscreen();
        }
    }

    fn toggle_media_fullscreen(&self) {
        if self.is_fullscreen() {
            self.unfullscreen();
        } else {
            self.fullscreen();
        }
    }

    fn show_media_context_menu(&self, x: f64, y: f64) {
        let viewer_ref = self.imp().media_viewer.borrow();
        let Some(viewer) = viewer_ref.as_ref() else {
            return;
        };
        if viewer.loaded_path.is_none() {
            return;
        }
        let popover = gtk::Popover::new();
        popover.set_parent(&viewer.content_stack);
        popover.set_has_arrow(true);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu.set_margin_top(6);
        menu.set_margin_bottom(6);
        menu.set_margin_start(6);
        menu.set_margin_end(6);
        let save = gtk::Button::with_label("Save As…");
        save.add_css_class("flat");
        let weak_window = self.downgrade();
        let popover_for_save = popover.clone();
        save.connect_clicked(move |_| {
            popover_for_save.popdown();
            if let Some(window) = weak_window.upgrade() {
                window.save_current_media();
            }
        });
        menu.append(&save);
        popover.set_child(Some(&menu));
        popover.popup();
    }

    fn save_current_media(&self) {
        let (source, name) = {
            let viewer_ref = self.imp().media_viewer.borrow();
            let Some(viewer) = viewer_ref.as_ref() else {
                return;
            };
            let Some(source) = viewer.loaded_path.clone() else {
                self.set_status("Media is still loading");
                return;
            };
            let name = viewer
                .gallery
                .get(viewer.index)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| "media".to_string());
            let name = PathBuf::from(name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("media")
                .to_string();
            (source, name)
        };
        let dialog = gtk::FileDialog::builder()
            .title("Save Media As")
            .initial_name(&name)
            .accept_label("Save")
            .modal(true)
            .build();
        let weak_window = self.downgrade();
        dialog.save(Some(self), None::<&gio::Cancellable>, move |result| {
            let Ok(destination) = result else {
                return;
            };
            if let Some(window) = weak_window.upgrade() {
                let source = gio::File::for_path(&source);
                let weak_window = window.downgrade();
                source.copy_async(
                    &destination,
                    gio::FileCopyFlags::OVERWRITE,
                    glib::Priority::DEFAULT,
                    None::<&gio::Cancellable>,
                    None,
                    move |result| {
                        if let Some(window) = weak_window.upgrade() {
                            match result {
                                Ok(()) => window.set_status("Media saved"),
                                Err(error) => {
                                    window.set_status(&format!("Could not save media: {error}"))
                                }
                            }
                        }
                    },
                );
            }
        });
    }

    fn present_loaded_media(&self, path: PathBuf, mime_type: &str) {
        let mut viewer_ref = self.imp().media_viewer.borrow_mut();
        let Some(viewer) = viewer_ref.as_mut() else {
            return;
        };
        viewer.loaded_path = Some(path.clone());
        if mime_type.starts_with("image/") {
            match gtk::gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    viewer.natural_size = (texture.width(), texture.height());
                    viewer.picture.set_paintable(Some(&texture));
                    viewer.content_stack.set_visible_child_name("image");
                    for button in [
                        &viewer.zoom_out_button,
                        &viewer.zoom_in_button,
                        &viewer.zoom_reset_button,
                    ] {
                        button.set_sensitive(true);
                    }
                    apply_media_zoom(viewer);
                    self.set_status("Image loaded");
                }
                Err(error) => self.set_status(&format!("Could not display image: {error}")),
            }
            return;
        }

        if let Some(existing) = viewer.content_stack.child_by_name("video") {
            viewer.content_stack.remove(&existing);
        }
        let file = gio::File::for_path(&path);
        let video = gtk::Video::for_file(Some(&file));
        video.set_autoplay(true);
        video.set_loop(false);
        video.set_hexpand(true);
        video.set_vexpand(true);
        let close_click = gtk::GestureClick::new();
        close_click.set_button(gtk::gdk::BUTTON_PRIMARY);
        let weak_window = self.downgrade();
        close_click.connect_released(move |_, presses, _, _| {
            if presses >= 2 {
                if let Some(window) = weak_window.upgrade() {
                    window.close_media_viewer();
                }
            }
        });
        video.add_controller(close_click);
        viewer.content_stack.add_named(&video, Some("video"));
        viewer.content_stack.set_visible_child_name("video");
        viewer.zoom_label.set_label("—");
        self.set_status("Video loaded");
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
        self.connect_close_request(|window| {
            window.flush_drafts();
            glib::Propagation::Proceed
        });
        self.connect_widget(&imp.connect_button.get(), |window| window.start_auth());
        self.connect_widget(&imp.messages_button.get(), |window| window.show_messages());
        self.connect_widget(&imp.unreads_button.get(), |window| window.show_unreads());
        self.connect_widget(&imp.files_button.get(), |window| window.show_files());
        self.connect_widget(&imp.refresh_button.get(), |window| {
            window.refresh_conversations()
        });
        self.connect_widget(&imp.saved_button.get(), |window| window.show_later());
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

        for buffer in [imp.message_entry.buffer(), imp.thread_entry.buffer()] {
            let weak_window = self.downgrade();
            buffer.connect_changed(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.schedule_draft_save();
                }
            });
        }

        let weak_window = self.downgrade();
        imp.message_search_entry.connect_activate(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.search_messages();
            }
        });

        imp.message_search_bar
            .connect_entry(&imp.message_search_entry.get());
        let weak_window = self.downgrade();
        imp.message_search_button.connect_toggled(move |button| {
            if let Some(window) = weak_window.upgrade() {
                window.set_workspace_search_visible(button.is_active());
            }
        });

        let weak_window = self.downgrade();
        imp.message_search_bar
            .connect_search_mode_enabled_notify(move |search_bar| {
                if let Some(window) = weak_window.upgrade() {
                    let button = window.imp().message_search_button.get();
                    if button.is_active() != search_bar.is_search_mode() {
                        button.set_active(search_bar.is_search_mode());
                    }
                }
            });

        let weak_window = self.downgrade();
        imp.thread_split.connect_show_sidebar_notify(move |split| {
            if !split.shows_sidebar() {
                if let Some(window) = weak_window.upgrade() {
                    if window.selected_thread_ts().is_some() {
                        window.close_thread();
                    }
                }
            }
        });
    }

    fn setup_settings(&self) {
        let settings = gio::Settings::new(config::APPLICATION_ID);
        *self.imp().drafts.borrow_mut() = DraftSettings::new(settings.clone()).load();
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

    fn draft_key(&self, channel_id: &str, thread_ts: Option<&str>) -> Option<DraftKey> {
        let workspace_id = self.imp().workspace_id.borrow().clone()?;
        Some(DraftKey::new(&workspace_id, channel_id, thread_ts))
    }

    fn schedule_draft_save(&self) {
        if self.visible_channel_id().is_none() {
            return;
        }
        let generation = self.imp().draft_save_generation.get().saturating_add(1);
        self.imp().draft_save_generation.set(generation);
        let weak_window = self.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            if window.imp().draft_save_generation.get() == generation {
                window.save_current_drafts();
            }
        });
    }

    fn flush_current_drafts(&self) {
        let generation = self.imp().draft_save_generation.get().saturating_add(1);
        self.imp().draft_save_generation.set(generation);
        self.save_current_drafts();
    }

    pub(crate) fn flush_drafts(&self) {
        self.flush_current_drafts();
    }

    fn save_current_drafts(&self) {
        let Some(channel_id) = self.visible_channel_id() else {
            return;
        };
        let Some(channel_key) = self.draft_key(&channel_id, None) else {
            return;
        };
        let thread_key = self
            .selected_thread_ts()
            .and_then(|thread_ts| self.draft_key(&channel_id, Some(&thread_ts)));
        let message_text = text_view_text(&self.imp().message_entry);
        let thread_text = text_view_text(&self.imp().thread_entry);
        let changed = {
            let mut drafts = self.imp().drafts.borrow_mut();
            let mut changed = drafts.upsert(channel_key, &message_text);
            if let Some(thread_key) = thread_key {
                changed |= drafts.upsert(thread_key, &thread_text);
            }
            changed
        };
        if changed {
            self.persist_drafts();
        }
    }

    fn persist_drafts(&self) {
        let Some(settings) = self.imp().settings.borrow().clone() else {
            return;
        };
        if let Err(error) = DraftSettings::new(settings).save(&self.imp().drafts.borrow()) {
            crate::debug::log("drafts", &format!("failed to persist drafts: {error}"));
        }
    }

    fn restore_channel_draft(&self, channel_id: &str) {
        let text = self
            .draft_key(channel_id, None)
            .and_then(|key| {
                self.imp()
                    .drafts
                    .borrow()
                    .get(&key)
                    .map(ToString::to_string)
            })
            .unwrap_or_default();
        set_text_view_text(&self.imp().message_entry, &text);
    }

    fn restore_thread_draft(&self, channel_id: &str, thread_ts: &str) {
        let text = self
            .draft_key(channel_id, Some(thread_ts))
            .and_then(|key| {
                self.imp()
                    .drafts
                    .borrow()
                    .get(&key)
                    .map(ToString::to_string)
            })
            .unwrap_or_default();
        set_text_view_text(&self.imp().thread_entry, &text);
    }

    fn remember_submitted_draft(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> bool {
        let Some(key) = self.draft_key(channel_id, thread_ts) else {
            return false;
        };
        record_draft_submission(&mut self.imp().pending_sent_drafts.borrow_mut(), key, text)
    }

    fn discard_submitted_draft(&self, channel_id: &str, thread_ts: Option<&str>) {
        if let Some(key) = self.draft_key(channel_id, thread_ts) {
            self.imp().pending_sent_drafts.borrow_mut().remove(&key);
        }
    }

    fn complete_submitted_draft(&self, channel_id: &str, thread_ts: Option<&str>) {
        let Some(key) = self.draft_key(channel_id, thread_ts) else {
            return;
        };
        let Some(submitted) = self.imp().pending_sent_drafts.borrow_mut().remove(&key) else {
            return;
        };

        let current_key = self.visible_channel_id().and_then(|visible_channel_id| {
            let visible_thread_ts = thread_ts.and_then(|_| self.selected_thread_ts());
            self.draft_key(&visible_channel_id, visible_thread_ts.as_deref())
        });
        let current_text = (current_key.as_ref() == Some(&key)).then(|| {
            if thread_ts.is_some() {
                text_view_text(&self.imp().thread_entry)
            } else {
                text_view_text(&self.imp().message_entry)
            }
        });
        let stored_text = self
            .imp()
            .drafts
            .borrow()
            .get(&key)
            .map(ToString::to_string);
        if !submitted_draft_matches(current_text.as_deref(), stored_text.as_deref(), &submitted) {
            return;
        }

        let stored_matches = stored_text.is_some_and(|text| text.trim() == submitted);
        if stored_matches && self.imp().drafts.borrow_mut().remove(&key) {
            self.persist_drafts();
        }
        if current_key.as_ref() == Some(&key) {
            if thread_ts.is_some() {
                set_text_view_text(&self.imp().thread_entry, "");
            } else {
                set_text_view_text(&self.imp().message_entry, "");
            }
        }
    }

    fn complete_upload_draft(&self, channel_id: &str, submitted: Option<&str>) {
        let Some(submitted) = submitted else {
            return;
        };
        let Some(key) = self.draft_key(channel_id, None) else {
            return;
        };
        let current_text = (self.visible_channel_id().as_deref() == Some(channel_id))
            .then(|| text_view_text(&self.imp().message_entry));
        let stored_text = self
            .imp()
            .drafts
            .borrow()
            .get(&key)
            .map(ToString::to_string);
        if !submitted_draft_matches(current_text.as_deref(), stored_text.as_deref(), submitted) {
            return;
        }

        if stored_text.is_some_and(|text| text.trim() == submitted)
            && self.imp().drafts.borrow_mut().remove(&key)
        {
            self.persist_drafts();
        }
        if current_text.is_some() {
            set_text_view_text(&self.imp().message_entry, "");
        }
    }

    fn setup_window_actions(&self) {
        self.add_window_action("sign-out", |window| {
            window.send_session_command(RuntimeCommand::SignOut)
        });
        self.add_window_action("switch-conversation", |window| {
            window.show_conversation_switcher()
        });
        self.add_window_action("search-workspace", |window| window.focus_workspace_search());
        self.add_window_action("show-messages", |window| window.show_messages());
        self.add_window_action("show-unreads", |window| window.show_unreads());
        self.add_window_action("show-files", |window| window.show_files());
        self.add_window_action("show-later", |window| window.show_later());
        self.add_window_action("refresh-conversations", |window| {
            window.refresh_conversations()
        });
        self.add_window_action("focus-composer", |window| window.focus_composer());
        self.add_window_action("upload-file", |window| window.choose_file_for_upload());
        self.add_window_action("close-thread", |window| window.close_thread());

        let shortcut_controller = gtk::ShortcutController::new();
        shortcut_controller.set_scope(gtk::ShortcutScope::Global);
        for shortcut in WINDOW_SHORTCUTS {
            for accelerator in shortcut.accelerators {
                let trigger = gtk::ShortcutTrigger::parse_string(accelerator)
                    .expect("window shortcut accelerator should be valid");
                let action = gtk::NamedAction::new(shortcut.action);
                shortcut_controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
            }
        }
        self.add_controller(shortcut_controller);

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
            RuntimeEventKind::Error(error) => {
                self.handle_runtime_error(&meta.context, &error);
            }
            RuntimeEventKind::RuntimeStartFailed(error) => self.show_session_error(&error),
            RuntimeEventKind::SignedOut => {
                self.imp().connect_requested.set(false);
                self.show_login("Choose a workspace to continue");
            }
            RuntimeEventKind::Authenticated(auth) => {
                if !self.imp().connect_requested.get() {
                    self.show_workspace(auth);
                    self.send_command(RuntimeCommand::DiscoverConversations);
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
            RuntimeEventKind::ConversationCandidatesLoaded { channels, users } => {
                let names = users
                    .iter()
                    .filter_map(|user| Some((user.id.clone()?, user.display_name()?)))
                    .collect::<HashMap<_, _>>();
                self.populate_user_names(names);
                *self.imp().discovered_channels.borrow_mut() = channels;
                *self.imp().discovered_users.borrow_mut() = users;
            }
            RuntimeEventKind::ConversationOpened(conversation) => {
                let channel_id = conversation.id.clone();
                let title = conversation.display_name_with_users(&self.imp().user_names.borrow());
                let mut conversations = self.imp().conversations.borrow().clone();
                if let Some(existing) = conversations
                    .iter_mut()
                    .find(|existing| existing.id == channel_id)
                {
                    *existing = conversation;
                } else {
                    conversations.push(conversation);
                }
                self.populate_conversations(conversations);
                self.select_conversation(&channel_id, &title);
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
            RuntimeEventKind::MessageContextLoaded { location, messages } => {
                let visible = self
                    .imp()
                    .workspace_view
                    .borrow_mut()
                    .apply_message_context(&location, messages);
                if visible {
                    if let Some(thread_ts) = location.thread_ts() {
                        let messages = self
                            .imp()
                            .workspace_view
                            .borrow()
                            .current_thread_messages()
                            .to_vec();
                        self.populate_thread(
                            location.channel_id(),
                            thread_ts,
                            messages,
                            TimelineScrollBehavior::Preserve,
                        );
                    } else {
                        let messages = self
                            .imp()
                            .workspace_view
                            .borrow()
                            .channel_messages(location.channel_id())
                            .to_vec();
                        self.populate_history_with_scroll(
                            location.channel_id(),
                            messages,
                            TimelineScrollBehavior::Preserve,
                        );
                    }
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
                self.mark_image_asset_failed(&key);
            }
            RuntimeEventKind::MediaLoaded {
                url,
                name: _,
                path,
                mime_type,
            } => {
                let is_current = self
                    .imp()
                    .media_viewer
                    .borrow()
                    .as_ref()
                    .and_then(|viewer| viewer.gallery.get(viewer.index))
                    .is_some_and(|item| item.url == url);
                if is_current {
                    self.present_loaded_media(path, &mime_type);
                }
            }
            RuntimeEventKind::MessagePosted {
                channel_id,
                message,
            } => {
                self.set_status("Message sent");
                let thread_ts = posted_message_thread_ts(&meta.context, &channel_id, &message);
                self.complete_submitted_draft(&channel_id, thread_ts.as_deref());
                if let Some(thread_ts) = thread_ts.as_deref() {
                    self.imp().thread_send_button.set_sensitive(true);
                    self.note_thread_reply_posted(&channel_id, thread_ts);
                } else {
                    self.imp().send_button.set_sensitive(true);
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
                self.set_status(&format!("Uploaded {name}"));
                let uploaded_channel = match &meta.context.target {
                    RuntimeTarget::Upload(channel_id) => Some(channel_id.as_str()),
                    _ => None,
                };
                if let Some(channel_id) = uploaded_channel {
                    let submitted = imp.pending_upload_drafts.borrow_mut().remove(channel_id);
                    self.complete_upload_draft(channel_id, submitted.flatten().as_deref());
                    if self.visible_channel_id().as_deref() == Some(channel_id) {
                        self.force_next_channel_bottom_render(channel_id);
                        self.request_channel_history(channel_id);
                    }
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

    fn show_messages(&self) {
        self.flush_current_drafts();
        if let Some(channel_id) = self.selected_channel_id() {
            let title = self.conversation_title(&channel_id);
            self.select_conversation(&channel_id, &title);
        } else {
            let title = gettext("Select a conversation");
            self.imp().workspace_view.borrow_mut().show_placeholder();
            self.imp().message_title.set_title(&title);
            self.show_message_placeholder(&title);
            self.render_closed_thread();
            self.render_conversations();
        }
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_unreads(&self) {
        self.flush_current_drafts();
        self.imp().workspace_view.borrow_mut().show_unreads();
        self.render_closed_thread();
        let items = self.unread_items();
        self.populate_unreads(items);
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_files(&self) {
        self.flush_current_drafts();
        let title = gettext("Files");
        self.imp().workspace_view.borrow_mut().start_files();
        self.render_closed_thread();
        self.imp().message_title.set_title(&title);
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(
            &title,
            &gettext("Loading files"),
        ));
        self.send_command(RuntimeCommand::LoadFiles);
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_later(&self) {
        self.flush_current_drafts();
        let title = gettext("Later");
        self.imp().workspace_view.borrow_mut().start_saved();
        self.imp().message_title.set_title(&title);
        self.render_closed_thread();
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(
            &title,
            &gettext("Loading saved items"),
        ));
        self.send_command(RuntimeCommand::LoadSavedItems);
        self.imp().workspace_split.set_show_content(true);
    }

    fn search_messages(&self) {
        let query = self.imp().message_search_entry.text().trim().to_string();
        if query.is_empty() {
            self.set_status("Enter a message search query");
            return;
        }
        self.flush_current_drafts();
        self.imp().workspace_view.borrow_mut().start_search();
        let title = gettext("Search results");
        self.render_closed_thread();
        self.render_conversations();
        self.imp().message_title.set_title(&title);
        self.load_message_html(&message_html::placeholder_document(
            &title,
            &gettext("Searching"),
        ));
        self.send_command(RuntimeCommand::SearchMessages { query });
        self.imp().workspace_split.set_show_content(true);
    }

    fn focus_workspace_search(&self) {
        self.imp().workspace_split.set_show_content(true);
        self.set_workspace_search_visible(true);
        let entry = self.imp().message_search_entry.get();
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    fn set_workspace_search_visible(&self, visible: bool) {
        let imp = self.imp();
        if imp.message_search_bar.is_search_mode() != visible {
            imp.message_search_bar.set_search_mode(visible);
        }
        if imp.message_search_button.is_active() != visible {
            imp.message_search_button.set_active(visible);
        }
        if visible {
            imp.message_search_entry.grab_focus();
        }
    }

    fn focus_composer(&self) {
        self.imp().workspace_split.set_show_content(true);
        let imp = self.imp();
        if imp.thread_split.shows_sidebar() {
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

        self.flush_current_drafts();
        if !self.remember_submitted_draft(&channel_id, None, &text) {
            self.set_status(&gettext("A message is already being sent."));
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

        self.flush_current_drafts();
        if !self.remember_submitted_draft(&channel_id, Some(&thread_ts), &text) {
            self.set_status(&gettext("A reply is already being sent."));
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
        if self
            .imp()
            .pending_upload_drafts
            .borrow()
            .contains_key(&channel_id)
        {
            self.set_status(&gettext("A file is already being uploaded here."));
            return;
        }
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
                        let initial_comment =
                            (!initial_comment.is_empty()).then(|| initial_comment.clone());
                        window.flush_current_drafts();
                        if !record_upload_submission(
                            &mut imp.pending_upload_drafts.borrow_mut(),
                            &channel_id,
                            initial_comment.clone(),
                        ) {
                            window.set_status(&gettext("A file is already being uploaded here."));
                            return;
                        }
                        imp.upload_button.set_sensitive(false);
                        imp.upload_progress.set_visible(true);
                        imp.upload_progress.set_fraction(0.0);
                        imp.upload_progress.set_text(Some("Starting upload"));
                        window.send_command(RuntimeCommand::UploadFile {
                            channel_id: channel_id.clone(),
                            path,
                            initial_comment,
                        });
                    }
                }
            }
        });
    }

    fn close_thread(&self) {
        self.flush_current_drafts();
        self.imp().workspace_view.borrow_mut().close_thread();
        self.render_closed_thread();
    }

    fn open_thread(&self, channel_id: &str, ts: &str) {
        self.flush_current_drafts();
        if self.visible_channel_id().as_deref() != Some(channel_id) {
            let title = self.conversation_title(channel_id);
            self.select_conversation(channel_id, &title);
        }
        let outcome = self
            .imp()
            .workspace_view
            .borrow_mut()
            .open_thread(channel_id, ts);
        self.restore_thread_draft(channel_id, ts);
        match outcome {
            ThreadOpenOutcome::RenderCurrent => {
                let messages = self
                    .imp()
                    .workspace_view
                    .borrow()
                    .current_thread_messages()
                    .to_vec();
                self.populate_thread(
                    channel_id,
                    ts,
                    messages,
                    TimelineScrollBehavior::StickToBottom,
                );
            }
            ThreadOpenOutcome::RequestFresh => {
                self.set_status(&gettext("Loading thread"));
                self.send_command(RuntimeCommand::LoadThread {
                    channel_id: channel_id.to_string(),
                    ts: ts.to_string(),
                });
            }
            ThreadOpenOutcome::AwaitFresh => self.set_status(&gettext("Loading thread")),
            ThreadOpenOutcome::Ignored => {}
        }
    }

    fn open_message_context(&self, location: SearchMessageLocation) {
        let channel_id = location.channel_id().to_string();
        let thread_ts = location.thread_ts().map(ToString::to_string);
        let title = self.conversation_title(&channel_id);
        self.select_conversation(&channel_id, &title);
        if let Some(thread_ts) = thread_ts.as_deref() {
            self.open_thread(&channel_id, thread_ts);
        }
        if !self
            .imp()
            .workspace_view
            .borrow_mut()
            .focus_message(&location)
        {
            return;
        }
        self.set_status(&gettext("Loading message context"));
        self.send_command(RuntimeCommand::LoadMessageContext(location));
    }

    fn render_closed_thread(&self) {
        let imp = self.imp();
        set_text_view_text(&imp.thread_entry, "");
        imp.thread_split.set_show_sidebar(false);
        self.load_thread_html(&message_html::placeholder_document(
            &gettext("Thread"),
            &gettext("No thread open"),
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
                self.open_thread(&channel_id, &ts);
                true
            }
            Some("message") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(message_ts) = query_param(url, "ts") else {
                    return true;
                };
                let Some(location) = SearchMessageLocation::new(
                    &channel_id,
                    &message_ts,
                    query_param(url, "thread_ts").as_deref(),
                ) else {
                    return true;
                };
                self.open_message_context(location);
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
            Some("unreads-open") => {
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
                self.remember_recent_reaction(&name);
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
            Some("forward") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                self.forward_message(&channel_id, &ts);
                true
            }
            Some("media") => {
                let Some(media_url) = query_param(url, "url").filter(|url| {
                    url::Url::parse(url)
                        .ok()
                        .is_some_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
                }) else {
                    return true;
                };
                let name = query_param(url, "name").unwrap_or_else(|| "Media".to_string());
                let kind = match query_param(url, "kind").as_deref() {
                    Some("image") => MediaKind::Image,
                    Some("video") => MediaKind::Video,
                    _ => return true,
                };
                self.open_media_viewer(MediaGalleryItem {
                    url: media_url,
                    name,
                    kind,
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

    fn forward_message(&self, channel_id: &str, ts: &str) {
        let Some(workspace_url) = self.imp().workspace_url.borrow().clone() else {
            self.set_status("Workspace URL is not available");
            return;
        };
        let Some(permalink) = message_permalink(&workspace_url, channel_id, ts) else {
            self.set_status("Could not build message link");
            return;
        };
        self.show_conversation_picker(
            "Forward message",
            "Choose a conversation",
            false,
            move |window, action| {
                window.send_command(RuntimeCommand::PostMessage {
                    channel_id: action.channel_id,
                    text: permalink.clone(),
                    thread_ts: None,
                });
                window.set_status("Forwarding message");
            },
        );
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
        self.flush_current_drafts();
        self.close_media_viewer();
        let imp = self.imp();
        imp.workspace_view.borrow_mut().reset();
        *imp.current_user_id.borrow_mut() = None;
        *imp.workspace_id.borrow_mut() = None;
        imp.workspace_ready.set(false);
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.pending_sent_drafts.borrow_mut().clear();
        imp.pending_upload_drafts.borrow_mut().clear();
        imp.conversations.borrow_mut().clear();
        imp.discovered_channels.borrow_mut().clear();
        imp.discovered_users.borrow_mut().clear();
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
        imp.send_button.set_sensitive(true);
        imp.thread_send_button.set_sensitive(true);
        imp.upload_button.set_sensitive(true);
        imp.upload_progress.set_visible(false);
        imp.upload_progress.set_fraction(0.0);
        imp.upload_progress.set_text(None);
        imp.sidebar_filter_entry.set_text("");
        imp.sidebar_unread_filter_button.set_active(false);
        imp.sidebar_all_filter_button.set_active(false);
        imp.workspace_title_label.set_title(&gettext("Workspace"));
        imp.workspace_status_label.set_label("");
        imp.message_status_label.set_label("");
        imp.workspace_split.set_show_content(false);
        imp.thread_split.set_show_sidebar(false);
        self.sync_workspace_chrome();
        self.clear_list(&imp.conversation_list);
        self.show_message_placeholder(&gettext("Select a conversation"));
        self.load_thread_html(&message_html::placeholder_document(
            &gettext("Thread"),
            &gettext("No thread open"),
        ));
    }

    fn show_workspace(&self, auth: AuthInfo) {
        *self.imp().workspace_id.borrow_mut() = workspace_identity(&auth);
        self.imp().workspace_ready.set(false);
        *self.imp().current_user_id.borrow_mut() = auth.user_id.clone();
        *self.imp().workspace_url.borrow_mut() = auth.url.clone();
        self.imp().connect_button.set_sensitive(true);
        let workspace_name = auth
            .team
            .or(auth.team_id)
            .unwrap_or_else(|| "Slack".to_string());
        *self.imp().workspace_name.borrow_mut() = Some(workspace_name.clone());
        self.imp().workspace_title_label.set_title(&workspace_name);
        self.set_status(&connected_workspace_status(Some(&workspace_name)));
        self.imp().content_stack.set_visible_child_name("workspace");
        self.imp().workspace_split.set_show_content(false);
        self.sync_workspace_chrome();
        if conversation_refresh_start_shows_sidebar_loading() {
            self.start_sidebar_loading();
        }
        self.activate_pending_notification_target();
    }

    fn set_status(&self, status: &str) {
        let imp = self.imp();
        imp.status_label.set_label(status);
        imp.connection_label.set_label(status);
        imp.workspace_status_label.set_label(status);
        imp.message_status_label.set_label(status);
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

    fn handle_runtime_error(&self, context: &OperationContext, error: &str) {
        match runtime_failure_recovery(context) {
            RuntimeFailureRecovery::Session => self.show_session_error(error),
            RuntimeFailureRecovery::Sidebar => self.show_conversation_load_error(error),
            RuntimeFailureRecovery::History(channel_id) => {
                let outcome = self
                    .imp()
                    .workspace_view
                    .borrow_mut()
                    .fail_history(&channel_id);
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::Messages, error);
                    }
                }
            }
            RuntimeFailureRecovery::Thread {
                channel_id,
                thread_ts,
            } => {
                let outcome = self
                    .imp()
                    .workspace_view
                    .borrow_mut()
                    .fail_thread(&channel_id, &thread_ts);
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_thread_error(error);
                    }
                }
            }
            RuntimeFailureRecovery::Search => {
                let outcome = self.imp().workspace_view.borrow_mut().fail_search();
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::SearchResults, error);
                    }
                }
            }
            RuntimeFailureRecovery::Files => {
                let outcome = self.imp().workspace_view.borrow_mut().fail_files();
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::Files, error);
                    }
                }
            }
            RuntimeFailureRecovery::SavedItems => {
                let outcome = self.imp().workspace_view.borrow_mut().fail_saved();
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::SavedItems, error);
                    }
                }
            }
            RuntimeFailureRecovery::User(user_id) => {
                self.imp().pending_user_ids.borrow_mut().remove(&user_id);
                crate::debug::log(
                    "ui",
                    &format!("UserLoadFailed user_id={user_id} error={error}"),
                );
            }
            RuntimeFailureRecovery::Image(key) => self.mark_image_asset_failed(&key),
            RuntimeFailureRecovery::Media => {
                self.set_status(error);
                self.close_media_viewer();
            }
            RuntimeFailureRecovery::PostMessage {
                channel_id,
                thread_ts,
            } => {
                self.discard_submitted_draft(&channel_id, thread_ts.as_deref());
                if thread_ts.is_some() {
                    self.imp().thread_send_button.set_sensitive(true);
                } else {
                    self.imp().send_button.set_sensitive(true);
                }
                if self.mutation_target_is_active(&channel_id, thread_ts.as_deref()) {
                    self.set_status(error);
                }
            }
            RuntimeFailureRecovery::Reaction {
                channel_id,
                thread_ts,
            }
            | RuntimeFailureRecovery::Saved {
                channel_id,
                thread_ts,
            } => {
                if self.mutation_target_is_active(&channel_id, thread_ts.as_deref()) {
                    self.set_status(error);
                }
            }
            RuntimeFailureRecovery::Upload(channel_id) => {
                let imp = self.imp();
                imp.pending_upload_drafts.borrow_mut().remove(&channel_id);
                imp.upload_button.set_sensitive(true);
                imp.upload_progress.set_visible(false);
                imp.upload_progress.set_fraction(0.0);
                imp.upload_progress.set_text(Some("Upload failed"));
                if self.mutation_target_is_active(&channel_id, None) {
                    self.set_status(error);
                }
            }
            RuntimeFailureRecovery::NonDisruptive => {
                crate::debug::log(
                    "ui",
                    &format!(
                        "RuntimeOperationFailed operation={:?} target={:?} error={error}",
                        context.operation, context.target
                    ),
                );
            }
        }
    }

    fn show_session_error(&self, error: &str) {
        self.show_login(error);
    }

    fn mutation_target_is_active(&self, channel_id: &str, thread_ts: Option<&str>) -> bool {
        let state = self.imp().workspace_view.borrow();
        mutation_target_is_active(
            state.visible_channel_id(),
            state.selected_thread_ts(),
            channel_id,
            thread_ts,
        )
    }

    fn show_main_surface_error(&self, surface: PlaceholderSurface, error: &str) {
        let title = surface.title();
        let message = surface.error_message(error);
        self.load_message_html(&message_html::placeholder_document(&title, &message));
    }

    fn show_thread_error(&self, error: &str) {
        let imp = self.imp();
        let title = gettext("Thread");
        imp.thread_title.set_title(&title);
        imp.thread_split.set_show_sidebar(true);
        let message = localized_replies_error(error);
        self.load_thread_html(&message_html::placeholder_document(&title, &message));
    }

    fn mark_image_asset_failed(&self, key: &str) {
        let imp = self.imp();
        imp.pending_image_assets.borrow_mut().remove(key);
        imp.failed_image_assets.borrow_mut().insert(key.to_string());
        self.rerender_current_messages();
    }

    fn show_conversation_load_error(&self, error: &str) {
        self.set_sidebar_error(error);
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
        if self.current_main_view() == MainMessageView::Unreads {
            self.populate_unreads(self.unread_items());
        } else {
            self.refresh_current_conversation_title();
        }
        self.imp().workspace_ready.set(true);
        self.activate_pending_notification_target();
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
            if self.current_main_view() == MainMessageView::Unreads {
                self.populate_unreads(self.unread_items());
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
        self.sync_workspace_chrome();
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
        self.show_conversation_picker(
            "Switch conversation",
            "Search conversations",
            true,
            |window, action| match action.action {
                ConversationPickerAction::OpenConversation => {
                    window.select_conversation(&action.channel_id, &action.title)
                }
                ConversationPickerAction::JoinChannel => {
                    window.send_command(RuntimeCommand::JoinConversation {
                        channel_id: action.channel_id,
                    });
                }
                ConversationPickerAction::OpenDirectMessage => {
                    window.send_command(RuntimeCommand::OpenDirectMessage {
                        user_id: action.channel_id,
                    });
                }
            },
        );
    }

    fn show_conversation_picker<F>(
        &self,
        title: &str,
        placeholder: &str,
        include_discovery: bool,
        on_activate: F,
    ) where
        F: Fn(&Self, SidebarRowAction) + 'static,
    {
        let imp = self.imp();
        let conversations = imp.conversations.borrow().clone();
        let user_names = imp.user_names.borrow().clone();
        let discovered_channels = imp.discovered_channels.borrow().clone();
        let discovered_users = imp.discovered_users.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        let sections = picker_sections(
            include_discovery,
            &conversations,
            &discovered_channels,
            &discovered_users,
            &user_names,
            current_user_id.as_deref(),
            "",
        );
        if picker_sections_empty(&sections) {
            self.set_status("No conversations loaded");
            return;
        }

        let dialog = gtk::Window::builder()
            .title(title)
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
        close_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
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
        search.set_placeholder_text(Some(placeholder));
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
        self.populate_conversation_picker_list(&list, &actions, &sections);

        let weak_window = self.downgrade();
        let list_for_search = list.clone();
        let actions_for_search = actions.clone();
        let conversations_for_search = conversations.clone();
        let user_names_for_search = user_names.clone();
        let discovered_channels_for_search = discovered_channels.clone();
        let discovered_users_for_search = discovered_users.clone();
        let current_user_id_for_search = current_user_id.clone();
        search.connect_search_changed(move |entry| {
            if let Some(window) = weak_window.upgrade() {
                let sections = picker_sections(
                    include_discovery,
                    &conversations_for_search,
                    &discovered_channels_for_search,
                    &discovered_users_for_search,
                    &user_names_for_search,
                    current_user_id_for_search.as_deref(),
                    entry.text().as_str(),
                );
                window.populate_conversation_picker_list(
                    &list_for_search,
                    &actions_for_search,
                    &sections,
                );
            }
        });

        let weak_window = self.downgrade();
        let actions_for_activate = actions.clone();
        let dialog_for_activate = dialog.clone();
        let on_activate = Rc::new(on_activate);
        list.connect_row_activated(move |_, row| {
            let action = sidebar_row_action_for_index(&actions_for_activate.borrow(), row.index());
            if let (Some(window), Some(action)) = (weak_window.upgrade(), action) {
                on_activate(&window, action);
                dialog_for_activate.close();
            }
        });

        dialog.present();
        search.grab_focus();
    }

    fn populate_conversation_picker_list(
        &self,
        list: &gtk::ListBox,
        actions: &Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
        sections: &ConversationPickerSections,
    ) {
        self.clear_list(list);
        actions.borrow_mut().clear();

        if picker_sections_empty(sections) {
            self.append_placeholder(list, "No matching conversations");
            return;
        }

        for (title, items) in [
            ("Conversations", sections.conversations.as_slice()),
            ("Channels", sections.channels.as_slice()),
            ("People", sections.people.as_slice()),
        ] {
            if items.is_empty() {
                continue;
            }
            self.append_picker_section_header(list, title);
            for item in items {
                self.append_conversation_picker_row(list, actions, item);
            }
        }
    }

    fn append_picker_section_header(&self, list: &gtk::ListBox, title: &str) {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_margin_top(10);
        label.set_margin_bottom(4);
        label.set_margin_start(9);
        label.set_margin_end(9);
        label.add_css_class("caption");
        label.add_css_class("heading");
        row.set_child(Some(&label));
        list.append(&row);
    }

    fn append_conversation_picker_row(
        &self,
        list: &gtk::ListBox,
        actions: &Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
        item: &ConversationPickerItem,
    ) {
        let row = sidebar_row_widget(&item.row, SidebarRowLayout::switcher());
        list.append(&row);
        actions
            .borrow_mut()
            .insert(row.index(), SidebarRowAction::from_picker_item(item));
    }

    fn refresh_current_conversation_title(&self) {
        let imp = self.imp();
        if self.current_main_view() == MainMessageView::Conversation {
            if let Some(channel_id) = self.visible_channel_id() {
                imp.message_title
                    .set_title(&self.conversation_title(&channel_id));
            }
        }
    }

    fn sync_workspace_chrome(&self) {
        let imp = self.imp();
        let main_view = imp.workspace_view.borrow().main_view();
        let selection = workspace_navigation_selection(main_view);
        imp.messages_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Messages));
        imp.unreads_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Unreads));
        imp.files_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Files));
        imp.saved_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Saved));
        imp.message_composer
            .set_visible(workspace_composer_visible(main_view));
    }

    fn select_conversation(&self, channel_id: &str, title: &str) {
        self.flush_current_drafts();
        crate::debug::log(
            "ui",
            &format!("select_conversation channel_id={channel_id} title={title}"),
        );
        let imp = self.imp();
        self.withdraw_conversation_notification(channel_id);
        let outcome = imp
            .workspace_view
            .borrow_mut()
            .select_conversation(channel_id);
        let current_messages = imp.workspace_view.borrow().snapshot().channel_messages;
        imp.message_title.set_title(title);
        self.restore_channel_draft(channel_id);
        set_text_view_text(&imp.thread_entry, "");
        imp.thread_split.set_show_sidebar(false);
        imp.workspace_split.set_show_content(true);
        self.load_thread_html(&message_html::placeholder_document(
            &gettext("Thread"),
            &gettext("No thread open"),
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
                    &gettext("Messages"),
                    &gettext("Loading messages"),
                ));
                self.send_command(RuntimeCommand::LoadHistory {
                    channel_id: channel_id.to_string(),
                });
            }
            ConversationSelectionDecision::AwaitFresh => {
                self.load_message_html(&message_html::placeholder_document(
                    &gettext("Messages"),
                    &gettext("Loading messages"),
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
            .set_title(&self.conversation_title(channel_id));
        let mut context = self.message_html_context(None);
        if !imp.workspace_view.borrow().has_channel_context(channel_id) {
            context.load_more_url = self.channel_load_more_url(channel_id);
        }
        context.timeline_scroll = scroll_behavior;
        let focus_message_ts = imp
            .workspace_view
            .borrow_mut()
            .take_channel_focus_for_render(channel_id, &messages);
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
        self.load_message_html(&message_html::conversation_document_with_focus(
            channel_id,
            &messages,
            &context,
            focus_message_ts.as_deref(),
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
        let title = gettext("Thread");
        imp.thread_title.set_title(&title);
        imp.thread_split.set_show_sidebar(true);

        if messages.is_empty() {
            self.load_thread_html(&message_html::placeholder_document(
                &title,
                &gettext("No replies"),
            ));
            return;
        }

        self.request_image_assets(messages.iter());
        let mut context = self.message_html_context(Some(ts));
        if !imp
            .workspace_view
            .borrow()
            .has_thread_context(channel_id, ts)
        {
            context.load_more_url = self.thread_load_more_url(channel_id, ts);
        }
        context.timeline_scroll = scroll_behavior;
        let focus_message_ts = imp
            .workspace_view
            .borrow_mut()
            .take_thread_focus_for_render(channel_id, ts, &messages);
        self.load_thread_html(&message_html::conversation_document_with_focus(
            channel_id,
            &messages,
            &context,
            focus_message_ts.as_deref(),
        ));
    }

    fn populate_unreads(&self, items: Vec<ActivityItem>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Unreads"));
        self.render_conversations();
        self.load_message_html(&message_html::unreads_document(&items));
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Search results"));
        let context = self.message_html_context(None);
        self.load_message_html(&message_html::search_results_document(&results, &context));
    }

    fn populate_files(&self, files: Vec<SlackFile>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Files"));
        self.render_conversations();
        self.load_message_html(&message_html::files_document(&files));
    }

    fn populate_saved_items(&self, items: Vec<SavedItem>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Later"));
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

        if outcome.refresh_unreads {
            self.populate_unreads(self.unread_items());
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
                channel_id,
                &self.conversation_title(channel_id),
                &message_notification_body(latest_message),
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

    fn send_notification(&self, channel_id: &str, title: &str, body: &str) {
        let Some(workspace_id) = self.imp().workspace_id.borrow().clone() else {
            return;
        };
        let Some(application) = self
            .application()
            .and_then(|application| application.downcast::<crate::ConduitApplication>().ok())
        else {
            return;
        };

        application.send_conversation_notification(&workspace_id, channel_id, title, body);
    }

    fn withdraw_conversation_notification(&self, channel_id: &str) {
        let Some(workspace_id) = self.imp().workspace_id.borrow().clone() else {
            return;
        };
        let Some(application) = self
            .application()
            .and_then(|application| application.downcast::<crate::ConduitApplication>().ok())
        else {
            return;
        };

        application.withdraw_conversation_notification(&workspace_id, channel_id);
    }

    pub(crate) fn open_notification_target(&self, workspace_id: String, channel_id: String) {
        *self.imp().pending_notification_target.borrow_mut() = Some(NotificationTarget {
            workspace_id,
            channel_id,
        });
        self.activate_pending_notification_target();
    }

    fn activate_pending_notification_target(&self) {
        let Some(target) = self.imp().pending_notification_target.borrow().clone() else {
            return;
        };
        let current_workspace_id = self.imp().workspace_id.borrow().clone();
        match notification_target_resolution(
            current_workspace_id.as_deref(),
            self.imp().workspace_ready.get(),
            &target,
        ) {
            NotificationTargetResolution::Wait => {}
            NotificationTargetResolution::RejectWorkspace => {
                self.imp().pending_notification_target.borrow_mut().take();
                self.set_status(&gettext(
                    "This notification belongs to a different workspace.",
                ));
            }
            NotificationTargetResolution::Open => {
                self.imp().pending_notification_target.borrow_mut().take();
                let title = self.conversation_title(&target.channel_id);
                self.select_conversation(&target.channel_id, &title);
                self.present();
            }
        }
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

    fn unread_items(&self) -> Vec<ActivityItem> {
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
        self.load_message_html(&message_html::placeholder_document(
            &gettext("Messages"),
            text,
        ));
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
            MainMessageView::Unreads => self.populate_unreads(self.unread_items()),
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
        let recent_reactions = imp
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.strv(config::RECENT_REACTIONS_KEY))
            .map(|names| names.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
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
            recent_reactions,
        }
    }

    fn remember_recent_reaction(&self, name: &str) {
        let settings = self.imp().settings.borrow().clone();
        let Some(settings) = settings else {
            return;
        };
        let stored = settings.strv(config::RECENT_REACTIONS_KEY);
        let names = promoted_recent_reactions(stored.iter().map(|value| value.as_str()), name);
        let values = names.iter().map(String::as_str).collect::<Vec<_>>();
        let _ = settings.set_strv(config::RECENT_REACTIONS_KEY, values);
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

    #[test]
    fn media_zoom_scales_below_fit_size_without_distorting_aspect_ratio() {
        assert_eq!(media_zoom_size((1600, 900), (800, 600), 1.0), (800, 450));
        assert_eq!(media_zoom_size((1600, 900), (800, 600), 0.5), (400, 225));
        assert_eq!(media_zoom_size((400, 200), (800, 600), 0.25), (100, 50));
    }
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

    #[test]
    fn runtime_failure_policy_maps_operations_and_targets_to_local_recovery() {
        let channel = RuntimeTarget::Channel("C123".to_string());
        let thread = RuntimeTarget::Thread {
            channel_id: "C123".to_string(),
            thread_ts: "1.0".to_string(),
        };
        let main_message = RuntimeTarget::Message {
            channel_id: "C123".to_string(),
            thread_ts: None,
        };
        let thread_message = RuntimeTarget::Message {
            channel_id: "C123".to_string(),
            thread_ts: Some("1.0".to_string()),
        };

        let cases = [
            (
                RuntimeOperation::Startup,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Session,
            ),
            (
                RuntimeOperation::Authenticate,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Session,
            ),
            (
                RuntimeOperation::SignOut,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Session,
            ),
            (
                RuntimeOperation::Disconnect,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Session,
            ),
            (
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Sidebar,
            ),
            (
                RuntimeOperation::History,
                channel.clone(),
                RuntimeFailureRecovery::History("C123".to_string()),
            ),
            (
                RuntimeOperation::OlderHistory,
                channel,
                RuntimeFailureRecovery::History("C123".to_string()),
            ),
            (
                RuntimeOperation::Thread,
                thread.clone(),
                RuntimeFailureRecovery::Thread {
                    channel_id: "C123".to_string(),
                    thread_ts: "1.0".to_string(),
                },
            ),
            (
                RuntimeOperation::OlderThread,
                thread,
                RuntimeFailureRecovery::Thread {
                    channel_id: "C123".to_string(),
                    thread_ts: "1.0".to_string(),
                },
            ),
            (
                RuntimeOperation::Search,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Search,
            ),
            (
                RuntimeOperation::Files,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::Files,
            ),
            (
                RuntimeOperation::SavedItems,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::SavedItems,
            ),
            (
                RuntimeOperation::User,
                RuntimeTarget::User("U123".to_string()),
                RuntimeFailureRecovery::User("U123".to_string()),
            ),
            (
                RuntimeOperation::ImageAsset,
                RuntimeTarget::Image("asset".to_string()),
                RuntimeFailureRecovery::Image("asset".to_string()),
            ),
            (
                RuntimeOperation::PostMessage,
                main_message.clone(),
                RuntimeFailureRecovery::PostMessage {
                    channel_id: "C123".to_string(),
                    thread_ts: None,
                },
            ),
            (
                RuntimeOperation::PostMessage,
                thread_message.clone(),
                RuntimeFailureRecovery::PostMessage {
                    channel_id: "C123".to_string(),
                    thread_ts: Some("1.0".to_string()),
                },
            ),
            (
                RuntimeOperation::Reaction,
                main_message.clone(),
                RuntimeFailureRecovery::Reaction {
                    channel_id: "C123".to_string(),
                    thread_ts: None,
                },
            ),
            (
                RuntimeOperation::Saved,
                main_message,
                RuntimeFailureRecovery::Saved {
                    channel_id: "C123".to_string(),
                    thread_ts: None,
                },
            ),
            (
                RuntimeOperation::FileUpload,
                RuntimeTarget::Upload("C123".to_string()),
                RuntimeFailureRecovery::Upload("C123".to_string()),
            ),
            (
                RuntimeOperation::SocketMode,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::NonDisruptive,
            ),
        ];

        for (operation, target, expected) in cases {
            let context = OperationContext::new(operation, target);
            assert_eq!(runtime_failure_recovery(&context), expected);
        }

        assert_eq!(
            runtime_failure_recovery(&OperationContext::new(
                RuntimeOperation::User,
                RuntimeTarget::Workspace,
            )),
            RuntimeFailureRecovery::NonDisruptive
        );
    }

    #[test]
    fn mutation_target_unreads_requires_the_channel_and_optional_thread() {
        assert!(mutation_target_is_active(
            Some("C1"),
            Some("T1"),
            "C1",
            None
        ));
        assert!(mutation_target_is_active(
            Some("C1"),
            Some("T1"),
            "C1",
            Some("T1")
        ));
        assert!(!mutation_target_is_active(
            Some("C2"),
            Some("T1"),
            "C1",
            None
        ));
        assert!(!mutation_target_is_active(
            Some("C1"),
            Some("T2"),
            "C1",
            Some("T1")
        ));
        assert!(!mutation_target_is_active(
            Some("C1"),
            None,
            "C1",
            Some("T1")
        ));
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
                action: ConversationPickerAction::OpenConversation,
            }
        );
    }

    #[test]
    fn sidebar_row_action_lookup_ignores_unregistered_rows() {
        let action = SidebarRowAction {
            channel_id: "C123".to_string(),
            title: "#general".to_string(),
            action: ConversationPickerAction::OpenConversation,
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
    fn localized_placeholder_error_templates_are_complete_per_surface() {
        for (surface, title, expected) in [
            (
                PlaceholderSurface::Messages,
                "Messages",
                "Could not load messages. Try again. token <expired>",
            ),
            (
                PlaceholderSurface::SearchResults,
                "Search results",
                "Could not load search results. Try again. token <expired>",
            ),
            (
                PlaceholderSurface::Files,
                "Files",
                "Could not load files. Try again. token <expired>",
            ),
            (
                PlaceholderSurface::SavedItems,
                "Later",
                "Could not load saved items. Try again. token <expired>",
            ),
        ] {
            assert_eq!(surface.title(), title);
            assert_eq!(surface.error_message("token <expired>"), expected);
        }

        assert_eq!(
            localized_replies_error("request failed"),
            "Could not load replies. Try again. request failed"
        );
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
    fn notification_targets_wait_for_the_workspace_and_conversations() {
        let target = NotificationTarget {
            workspace_id: "T123".into(),
            channel_id: "C123".into(),
        };

        assert_eq!(
            notification_target_resolution(None, false, &target),
            NotificationTargetResolution::Wait
        );
        assert_eq!(
            notification_target_resolution(Some("T123"), false, &target),
            NotificationTargetResolution::Wait
        );
        assert_eq!(
            notification_target_resolution(Some("T123"), true, &target),
            NotificationTargetResolution::Open
        );
        assert_eq!(
            notification_target_resolution(Some("T999"), true, &target),
            NotificationTargetResolution::RejectWorkspace
        );
    }

    #[test]
    fn workspace_identity_prefers_stable_team_id_with_fallbacks() {
        assert_eq!(
            workspace_identity(&AuthInfo {
                team_id: Some(" T123 ".into()),
                user_id: Some(" U123 ".into()),
                url: Some("https://workspace.slack.com".into()),
                team: Some("Workspace".into()),
                ..Default::default()
            }),
            Some("T123:U123".into())
        );
        assert_eq!(
            workspace_identity(&AuthInfo {
                url: Some("https://workspace.slack.com".into()),
                ..Default::default()
            }),
            Some("https://workspace.slack.com".into())
        );
        assert_eq!(workspace_identity(&AuthInfo::default()), None);
    }

    #[test]
    fn sent_drafts_clear_only_while_the_submitted_text_is_unchanged() {
        assert!(submitted_draft_matches(
            Some(" hello \n"),
            Some("hello"),
            "hello"
        ));
        assert!(submitted_draft_matches(None, Some("hello"), "hello"));
        assert!(!submitted_draft_matches(
            Some("hello, edited"),
            Some("hello"),
            "hello"
        ));
        assert!(!submitted_draft_matches(None, None, "hello"));

        let context = OperationContext::new(
            RuntimeOperation::PostMessage,
            RuntimeTarget::Message {
                channel_id: "C123".into(),
                thread_ts: Some("parent".into()),
            },
        );
        assert_eq!(
            posted_message_thread_ts(&context, "C123", &SlackMessage::default()).as_deref(),
            Some("parent")
        );
    }

    #[test]
    fn only_one_submission_can_be_in_flight_for_each_draft() {
        let key = DraftKey::new("T123:U123", "C123", None);
        let other = DraftKey::new("T123:U123", "C999", None);
        let mut pending = HashMap::new();

        assert!(record_draft_submission(&mut pending, key.clone(), "first"));
        assert!(!record_draft_submission(&mut pending, key, "duplicate"));
        assert!(record_draft_submission(&mut pending, other, "parallel"));
        assert_eq!(pending.len(), 2);

        let mut uploads = HashMap::new();
        assert!(record_upload_submission(
            &mut uploads,
            "C123",
            Some("comment".into())
        ));
        assert!(!record_upload_submission(
            &mut uploads,
            "C123",
            Some("replacement".into())
        ));
        assert!(record_upload_submission(&mut uploads, "C999", None));
    }

    #[test]
    fn workspace_navigation_selection_follows_authoritative_main_view() {
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Conversation),
            Some(WorkspaceNavigationSelection::Messages)
        );
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Unreads),
            Some(WorkspaceNavigationSelection::Unreads)
        );
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Files),
            Some(WorkspaceNavigationSelection::Files)
        );
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Saved),
            Some(WorkspaceNavigationSelection::Saved)
        );
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Placeholder),
            None
        );
        assert_eq!(
            workspace_navigation_selection(MainMessageView::Search),
            None
        );
    }

    #[test]
    fn composer_is_only_visible_for_conversations() {
        assert!(workspace_composer_visible(MainMessageView::Conversation));
        for view in [
            MainMessageView::Placeholder,
            MainMessageView::Unreads,
            MainMessageView::Search,
            MainMessageView::Files,
            MainMessageView::Saved,
        ] {
            assert!(!workspace_composer_visible(view));
        }
    }

    #[test]
    fn window_template_uses_adaptive_accessible_shell() {
        let template = include_str!("window.ui");

        for required in [
            "AdwNavigationSplitView\" id=\"workspace_split",
            "AdwOverlaySplitView\" id=\"thread_split",
            "AdwNavigationPage",
            "GtkSearchBar\" id=\"message_search_bar",
            "AdwClamp",
            "GtkScrolledWindow\" id=\"auth_scroller",
            "<property name=\"vscrollbar-policy\">automatic</property>",
            "AdwEntryRow\" id=\"client_id_entry",
            "AdwPasswordEntryRow\" id=\"xoxc_token_entry",
            "AdwPasswordEntryRow\" id=\"xoxd_token_entry",
            "<property name=\"label\" translatable=\"yes\">Message</property>",
            "<property name=\"label\" translatable=\"yes\">Reply</property>",
            "GtkToggleButton\" id=\"messages_button",
            "<property name=\"group\">messages_button</property>",
            "<property name=\"icon-name\">view-list-symbolic</property>",
            "<property name=\"icon-name\">mail-unread-symbolic</property>",
            "<property name=\"tooltip-text\" translatable=\"yes\">Messages</property>",
            "<property name=\"tooltip-text\" translatable=\"yes\">Unreads</property>",
            "<property name=\"enable-show-gesture\">False</property>",
            "GtkLabel\" id=\"message_status_label",
            "<property name=\"accessible-role\">status</property>",
        ] {
            assert!(
                template.contains(required),
                "missing template marker {required}"
            );
        }

        assert!(!template.contains("<object class=\"GtkPaned\""));
        assert!(!template.contains("<property name=\"width-request\">460</property>"));
        assert!(!template.contains("<property name=\"width-request\">280</property>"));
        assert!(!template.contains("<property name=\"width-request\">220</property>"));
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

    #[test]
    fn recent_reactions_are_promoted_deduplicated_and_bounded() {
        assert_eq!(
            promoted_recent_reactions(["thumbsup", "heart", "eyes", "fire"], "heart"),
            vec!["heart", "thumbsup", "eyes"]
        );
        assert_eq!(
            promoted_recent_reactions(["thumbsup", "heart"], "rocket"),
            vec!["rocket", "thumbsup", "heart"]
        );
    }
}
