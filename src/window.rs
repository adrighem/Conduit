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
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::activity::{self, ActivityItem};
use crate::auth;
use crate::composer::{
    emoji_completion_key_action, emoji_token_at_caret, replace_emoji_token, set_text_view_text,
    text_view_enter_action, text_view_text, EmojiCompletionKeyAction, EmojiToken,
    TextViewEnterAction,
};
use crate::config;
use crate::drafts::{DraftKey, DraftSettings, Drafts};
use crate::emoji::{
    emoji_picker_accessible_label, move_emoji_picker_selection, EmojiCatalog, EmojiEntry,
    EmojiPickerModel, EmojiPickerMove, EmojiValue,
};
use crate::message_html::{
    self, MessageHtmlContext, TimelineDomPatch, TimelineInsertPosition, TimelineMessageRegion,
    TimelineScrollBehavior,
};
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation, SlackFile,
    SlackMessage, SlackUnreadState, SlackUser, SlackUserStatus,
};
use crate::rendering;
use crate::runtime::{
    AppRuntime, OperationContext, RequestId, RuntimeCommand, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimeFailure, RuntimeFailureCategory, RuntimeIdentity, RuntimeOperation,
    RuntimeTarget, SessionId,
};
use crate::shortcuts::WINDOW_SHORTCUTS;
use crate::sidebar::{
    self, diff_keyed_sidebar_items, ConversationPickerAction, ConversationPickerItem,
    ConversationPickerSections, KeyedSidebarItem, SidebarItemKey, SidebarItemModel,
    SidebarRowModel,
};
use crate::sidebar_widgets::{sidebar_row_widget, SidebarRowLayout};
use crate::socket_mode::{
    SocketModeEvent, SocketModeMessageEvent, SocketModeMessageKind, SocketModeReactionEvent,
};
use crate::thread_catalog::ThreadCatalog;
use crate::thread_pane::ThreadPane;
use crate::workspace_state::{
    ConversationSelectionDecision, MainMessageView, ReactionUpdate, RealtimeMessageKind,
    ThreadApplyOutcome, ThreadOpenOutcome, WorkspaceLifecycle, WorkspaceLifecycleEvent,
    WorkspaceScrollBehavior, WorkspaceSessionState, WorkspaceSnapshot,
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
        pub threads_button: TemplateChild<gtk::ToggleButton>,
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
        pub conversation_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub workspace_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub message_pane: TemplateChild<gtk::Box>,
        #[template_child]
        pub message_view_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub message_composer: TemplateChild<gtk::Box>,
        #[template_child]
        pub thread_split: TemplateChild<adw::OverlaySplitView>,
        #[template_child]
        pub thread_resize_handle: TemplateChild<gtk::Separator>,
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
        pub thread_pane: TemplateChild<gtk::Box>,
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
        pub(super) workspace: WorkspaceSessionState,
        pub pending_opened_conversation_ids: RefCell<HashSet<String>>,
        pub discovered_channels: RefCell<Vec<SlackConversation>>,
        pub discovered_users: RefCell<Vec<SlackUser>>,
        pub(super) conversation_picker_view: RefCell<Option<ConversationPickerView>>,
        pub(super) sidebar_row_actions: RefCell<HashMap<i32, SidebarRowAction>>,
        pub latest_message_ts_by_channel: RefCell<HashMap<String, String>>,
        pub local_read_ts_by_channel: RefCell<HashMap<String, String>>,
        pub seen_realtime_messages: RefCell<HashSet<String>>,
        pub user_names: RefCell<HashMap<String, String>>,
        pub user_full_names: RefCell<HashMap<String, String>>,
        pub user_search_aliases: RefCell<sidebar::UserSearchAliases>,
        pub user_statuses: RefCell<sidebar::UserStatuses>,
        pub status_expiry_generation: Cell<u64>,
        pub user_group_names: RefCell<HashMap<String, String>>,
        pub user_group_members: RefCell<HashMap<String, Vec<String>>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub pending_profile_user_id: RefCell<Option<String>>,
        pub workspace_id: RefCell<Option<String>>,
        pub workspace_name: RefCell<Option<String>>,
        pub workspace_url: RefCell<Option<String>>,
        pub workspace_ready: Cell<bool>,
        pub(super) pending_notification_target: RefCell<Option<NotificationTarget>>,
        pub drafts: RefCell<Drafts>,
        pub draft_save_generation: Cell<u64>,
        pub pending_sent_drafts: RefCell<HashMap<DraftKey, String>>,
        pub pending_upload_drafts: RefCell<HashMap<DraftKey, Option<String>>>,
        pub sidebar_loading: Cell<bool>,
        pub sidebar_error: RefCell<Option<String>>,
        pub current_user_id: RefCell<Option<String>>,
        pub message_view: RefCell<Option<webkit6::WebView>>,
        pub(super) media_viewer: RefCell<Option<MediaViewer>>,
        pub(super) thread_pane_controller: RefCell<Option<ThreadPane>>,
        pub image_assets: RefCell<HashMap<String, String>>,
        pub pending_image_assets: RefCell<HashSet<String>>,
        pub failed_image_assets: RefCell<HashSet<String>>,
        pub custom_emojis: RefCell<HashMap<String, String>>,
        pub(super) message_emoji_completion: RefCell<Option<ComposerEmojiCompletion>>,
        pub(super) thread_emoji_completion: RefCell<Option<ComposerEmojiCompletion>>,
        pub(super) pending_ui_invalidations: Cell<UiInvalidations>,
        pub(super) sidebar_items: RefCell<Vec<KeyedSidebarItem>>,
        pub(super) sidebar_rows: RefCell<HashMap<SidebarItemKey, gtk::ListBoxRow>>,
        pub(super) sidebar_filter_generation: Cell<u64>,
        pub(super) picker_filter_generation: Cell<u64>,
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
                obj.select_conversation("C_TEST", "#general");
                if std::env::var_os("CONDUIT_TEST_THREAD_COMPOSER").is_some() {
                    obj.imp().thread_split.set_show_sidebar(true);
                }
            } else {
                obj.show_loading("Checking secure storage");
                obj.send_session_command(RuntimeCommand::LoadStoredToken);
            }
        }

        fn dispose(&self) {
            // These popovers are manually parented to GtkTextView so they can
            // point at the composer caret. Detach them before the template
            // children are disposed; GtkTextView cannot remove unregistered
            // direct children itself and otherwise loops while warning.
            for completion in [
                &self.message_emoji_completion,
                &self.thread_emoji_completion,
            ] {
                if let Some(completion) = completion.borrow_mut().take() {
                    completion.popover.popdown();
                    if completion.popover.parent().is_some() {
                        completion.popover.unparent();
                    }
                }
            }
        }
    }

    impl WidgetImpl for ConduitWindow {}
    impl WindowImpl for ConduitWindow {}
    impl ApplicationWindowImpl for ConduitWindow {}
    impl AdwApplicationWindowImpl for ConduitWindow {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerTarget {
    Message,
    Thread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineSurface {
    Main,
    Thread,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UiInvalidations(u8);

impl UiInvalidations {
    const SIDEBAR: Self = Self(1 << 0);
    const MAIN: Self = Self(1 << 1);
    const THREAD: Self = Self(1 << 2);
    const TITLE: Self = Self(1 << 3);
    const PICKER: Self = Self(1 << 4);

    fn contains(self, invalidation: Self) -> bool {
        self.0 & invalidation.0 != 0
    }

    fn insert(&mut self, invalidations: Self) -> bool {
        let was_empty = self.0 == 0;
        self.0 |= invalidations.0;
        was_empty
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl std::ops::BitOr for UiInvalidations {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

fn generate_html(label: &str, render: impl FnOnce() -> String) -> String {
    let started = Instant::now();
    let html = render();
    log_performance(started, |elapsed_ms| {
        format!(
            "html_generation surface={label} bytes={} elapsed_ms={:.2}",
            html.len(),
            elapsed_ms
        )
    });
    html
}

fn log_performance(started: Instant, message: impl FnOnce(f64) -> String) {
    if crate::debug::enabled() {
        crate::debug::log(
            "performance",
            &message(started.elapsed().as_secs_f64() * 1_000.0),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationPanePasteFocus {
    MainPane,
    ThreadPane,
    Composer,
    TextInput,
    Outside,
}

fn conversation_pane_image_paste_target(
    focus: ConversationPanePasteFocus,
    clipboard_has_image: bool,
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
) -> Option<ComposerTarget> {
    if !clipboard_has_image || !is_unmodified_paste_accelerator(key, state) {
        return None;
    }
    match focus {
        ConversationPanePasteFocus::MainPane => Some(ComposerTarget::Message),
        ConversationPanePasteFocus::ThreadPane => Some(ComposerTarget::Thread),
        ConversationPanePasteFocus::Composer
        | ConversationPanePasteFocus::TextInput
        | ConversationPanePasteFocus::Outside => None,
    }
}

fn is_unmodified_paste_accelerator(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    matches!(key, gtk::gdk::Key::v | gtk::gdk::Key::V)
        && state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
        && !state.intersects(
            gtk::gdk::ModifierType::SHIFT_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK
                | gtk::gdk::ModifierType::META_MASK,
        )
}

const COMPOSER_TARGETS: [ComposerTarget; 2] = [ComposerTarget::Message, ComposerTarget::Thread];
const UI_EVENT_BATCH_LIMIT: usize = 8;

#[derive(Debug)]
struct ComposerEmojiCompletion {
    popover: gtk::Popover,
    list: gtk::ListBox,
    entries: Vec<EmojiEntry>,
    token: Option<EmojiToken>,
}

fn composer_emoji_preview(entry: &EmojiEntry) -> gtk::Widget {
    match &entry.value {
        EmojiValue::Unicode(value) => {
            let preview = gtk::Label::new(Some(value));
            preview.add_css_class("title-3");
            preview.upcast()
        }
        EmojiValue::CustomImage(url) => {
            let preview = gtk::Picture::for_file(&gio::File::for_uri(url));
            preview.set_alternative_text(Some(&entry.label));
            preview.set_can_shrink(true);
            preview.set_content_fit(gtk::ContentFit::Contain);
            preview.set_size_request(24, 24);
            preview.upcast()
        }
    }
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

#[derive(Debug, Clone)]
struct ConversationPickerView {
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    actions: Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
    include_discovery: bool,
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
    image: gtk::DrawingArea,
    image_source: Rc<RefCell<Option<gdk_pixbuf::Pixbuf>>>,
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
    source: sidebar::ConversationPickerSource<'_>,
    query: &str,
) -> ConversationPickerSections {
    let sidebar::ConversationPickerSource {
        conversations,
        discovered_channels,
        discovered_users,
        user_names,
        current_user_id,
        known_user_search_aliases,
        user_full_names,
        user_statuses,
    } = source;
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
    sidebar::conversation_picker_sections_with_statuses(
        sidebar::ConversationPickerSource {
            conversations,
            discovered_channels: channels,
            discovered_users: users,
            user_names,
            current_user_id,
            known_user_search_aliases,
            user_full_names,
            user_statuses,
        },
        query,
    )
}

fn picker_sections_empty(sections: &ConversationPickerSections) -> bool {
    sections.search_results.as_ref().map_or_else(
        || {
            sections.conversations.is_empty()
                && sections.channels.is_empty()
                && sections.people.is_empty()
        },
        Vec::is_empty,
    )
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
    viewer.image.set_content_width(width);
    viewer.image.set_content_height(height);
    viewer.image.queue_resize();
    viewer.image.queue_draw();
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
    Threads,
    Files,
    Saved,
}

fn workspace_navigation_selection(
    main_view: MainMessageView,
) -> Option<WorkspaceNavigationSelection> {
    match main_view {
        MainMessageView::Conversation => Some(WorkspaceNavigationSelection::Messages),
        MainMessageView::Unreads => Some(WorkspaceNavigationSelection::Unreads),
        MainMessageView::Threads => Some(WorkspaceNavigationSelection::Threads),
        MainMessageView::Files => Some(WorkspaceNavigationSelection::Files),
        MainMessageView::Saved => Some(WorkspaceNavigationSelection::Saved),
        MainMessageView::Placeholder | MainMessageView::Search => None,
    }
}

fn workspace_composer_visible(main_view: MainMessageView) -> bool {
    main_view == MainMessageView::Conversation
}

fn sidebar_conversation_can_leave(conversation: &SlackConversation) -> bool {
    !conversation.is_im.unwrap_or(false)
        && !conversation.is_mpim.unwrap_or(false)
        && (conversation.is_channel.unwrap_or(false)
            || conversation.is_group.unwrap_or(false)
            || conversation.is_private.unwrap_or(false))
        && !conversation.is_archived.unwrap_or(false)
}

fn sidebar_conversation_leave_requires_confirmation(conversation: &SlackConversation) -> bool {
    sidebar_conversation_can_leave(conversation)
        && (conversation.is_private.unwrap_or(false) || conversation.is_group.unwrap_or(false))
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
enum MessageNotificationDelivery {
    Snapshot,
    Realtime { first_delivery: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MessageNotificationState<'a> {
    previous_latest_ts: Option<&'a str>,
    latest_ts: &'a str,
    latest_message_user: Option<&'a str>,
    current_user: Option<&'a str>,
    has_unread: bool,
    muted: bool,
    actively_reading: bool,
    delivery: MessageNotificationDelivery,
}

fn message_notification_action(state: MessageNotificationState<'_>) -> MessageNotificationAction {
    let has_newer_message = match state.delivery {
        MessageNotificationDelivery::Realtime { first_delivery } => first_delivery,
        MessageNotificationDelivery::Snapshot => state
            .previous_latest_ts
            .is_some_and(|previous_ts| state.latest_ts > previous_ts),
    };
    let own_message = state
        .latest_message_user
        .is_some_and(|user| Some(user) == state.current_user);

    if has_newer_message
        && state.has_unread
        && !state.muted
        && !state.actively_reading
        && !own_message
    {
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

fn notification_baseline_after(previous_ts: Option<&str>, candidate_ts: &str) -> String {
    previous_ts
        .filter(|previous_ts| *previous_ts >= candidate_ts)
        .unwrap_or(candidate_ts)
        .to_string()
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
    pending: &mut HashMap<DraftKey, Option<String>>,
    key: DraftKey,
    initial_comment: Option<String>,
) -> bool {
    if pending.contains_key(&key) {
        return false;
    }
    pending.insert(key, initial_comment);
    true
}

fn clipboard_formats_include_image(formats: &gtk::gdk::ContentFormats) -> bool {
    formats.contains_type(gtk::gdk::Texture::static_type())
        || formats
            .mime_types()
            .iter()
            .any(|mime_type| clipboard_mime_type_is_image(mime_type))
}

fn clipboard_mime_type_is_image(mime_type: &str) -> bool {
    mime_type
        .split(';')
        .next()
        .is_some_and(|mime_type| mime_type.trim().starts_with("image/"))
}

fn screenshot_filename() -> String {
    let timestamp = glib::DateTime::now_local()
        .ok()
        .and_then(|date_time| date_time.format("%Y-%m-%d_%H-%M-%S").ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "clipboard".to_string());
    format!("Screenshot-{timestamp}-{:08x}.png", rand::random::<u32>())
}

fn clear_stale_upload_staging() {
    let directory = config::upload_staging_dir();
    if let Err(error) = std::fs::remove_dir_all(&directory) {
        if error.kind() != std::io::ErrorKind::NotFound {
            crate::debug::log(
                "ui",
                &format!(
                    "StaleUploadCleanupFailed path={} error={error}",
                    directory.display()
                ),
            );
        }
    }
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
    Attachment,
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
    Upload {
        channel_id: String,
        thread_ts: Option<String>,
    },
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
        (RuntimeOperation::AttachmentDownload, RuntimeTarget::Attachment(_)) => {
            RuntimeFailureRecovery::Attachment
        }
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
        (
            RuntimeOperation::FileUpload,
            RuntimeTarget::Upload {
                channel_id,
                thread_ts,
            },
        ) => RuntimeFailureRecovery::Upload {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
        },
        _ => RuntimeFailureRecovery::NonDisruptive,
    }
}

fn runtime_failure_recovery_for_failure(
    context: &OperationContext,
    failure: &RuntimeFailure,
) -> RuntimeFailureRecovery {
    if failure.category == RuntimeFailureCategory::Authentication {
        RuntimeFailureRecovery::Session
    } else {
        runtime_failure_recovery(context)
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

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

fn nearest_status_expiration(statuses: &HashMap<String, SlackUserStatus>, now: i64) -> Option<i64> {
    statuses
        .values()
        .map(|status| status.expiration)
        .filter(|expiration| *expiration > now)
        .min()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceLifecycleSurface {
    Connect,
    Loading,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceLifecyclePresentation {
    surface: WorkspaceLifecycleSurface,
    status: &'static str,
}

fn workspace_lifecycle_presentation(
    lifecycle: WorkspaceLifecycle,
    workspace_available: bool,
) -> WorkspaceLifecyclePresentation {
    use WorkspaceLifecycle as Lifecycle;
    use WorkspaceLifecycleSurface as Surface;

    match lifecycle {
        Lifecycle::Disconnected => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Choose a workspace to continue",
        },
        Lifecycle::Connecting => WorkspaceLifecyclePresentation {
            surface: Surface::Loading,
            status: "Connecting to Slack…",
        },
        Lifecycle::Syncing => WorkspaceLifecyclePresentation {
            surface: if workspace_available {
                Surface::Workspace
            } else {
                Surface::Loading
            },
            status: "Syncing workspace…",
        },
        Lifecycle::Ready => WorkspaceLifecyclePresentation {
            surface: Surface::Workspace,
            status: "",
        },
        Lifecycle::Degraded => WorkspaceLifecyclePresentation {
            surface: if workspace_available {
                Surface::Workspace
            } else {
                Surface::Connect
            },
            status: "Connection interrupted. Retrying…",
        },
        Lifecycle::AuthenticationRequired => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Slack authentication failed. Sign in again.",
        },
        Lifecycle::StartupFailed => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Conduit could not start.",
        },
    }
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
    let url = if file.supported_media_kind() == Some("video") {
        file.video_preview_url()?
    } else {
        file.preview_url()?
    };
    Some((url.to_string(), url.to_string()))
}

fn messages_use_image_asset(messages: &[SlackMessage], key: &str) -> bool {
    messages
        .iter()
        .flat_map(|message| message.files.as_ref().into_iter().flatten())
        .filter_map(image_asset_request)
        .any(|(candidate, _)| candidate == key)
}

fn messages_use_user(messages: &[SlackMessage], user_id: &str) -> bool {
    messages.iter().any(|message| {
        rendering::extract_user_ids(message)
            .iter()
            .any(|id| id == user_id)
    })
}

fn messages_use_user_in_reactions(messages: &[SlackMessage], user_id: &str) -> bool {
    messages.iter().any(|message| {
        message
            .reactions
            .as_ref()
            .into_iter()
            .flatten()
            .any(|reaction| {
                reaction
                    .users
                    .as_ref()
                    .is_some_and(|users| users.iter().any(|id| id == user_id))
            })
    })
}

fn realtime_dom_patch_kind(
    kind: RealtimeMessageKind,
    current_messages: &[SlackMessage],
    message: &SlackMessage,
) -> Option<RealtimeMessageKind> {
    if kind != RealtimeMessageKind::Posted {
        return Some(kind);
    }

    if current_messages
        .iter()
        .any(|current| current.ts == message.ts)
    {
        // Socket Mode may redeliver an event, or the same message may already have
        // arrived through a history refresh. Replace it instead of duplicating it.
        return Some(RealtimeMessageKind::Changed);
    }

    current_messages
        .first()
        .is_none_or(|newest| message.ts > newest.ts)
        .then_some(RealtimeMessageKind::Posted)
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

fn slack_timestamp_from_permalink(value: &str) -> Option<String> {
    let digits = value.strip_prefix('p').unwrap_or(value);
    if digits.len() <= 6 || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let split = digits.len() - 6;
    Some(format!("{}.{}", &digits[..split], &digits[split..]))
}

fn slack_message_location(uri: &str, workspace_url: Option<&str>) -> Option<SearchMessageLocation> {
    let workspace_url = url::Url::parse(workspace_url?).ok()?;
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str()? != workspace_url.host_str()?
        || !url.host_str()?.ends_with(".slack.com")
    {
        return None;
    }

    let mut segments = url.path_segments()?;
    if segments.next()? != "archives" {
        return None;
    }
    let channel_id = segments.next()?;
    if channel_id.is_empty()
        || !channel_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return None;
    }
    let message_ts = slack_timestamp_from_permalink(segments.next()?)?;
    if segments.next().is_some() {
        return None;
    }
    let thread_ts = match query_param(&url, "thread_ts") {
        Some(thread_ts) => {
            let normalized = if let Some((seconds, fraction)) = thread_ts.split_once('.') {
                (!seconds.is_empty()
                    && fraction.len() == 6
                    && seconds.chars().all(|character| character.is_ascii_digit())
                    && fraction.chars().all(|character| character.is_ascii_digit()))
                .then_some(thread_ts)?
            } else {
                slack_timestamp_from_permalink(&thread_ts)?
            };
            Some(normalized)
        }
        None => None,
    };
    SearchMessageLocation::new(channel_id, &message_ts, thread_ts.as_deref())
}

fn realtime_message_marks_unread(
    _selected_channel: Option<&str>,
    _window_active: bool,
    current_user_id: Option<&str>,
    event: &SocketModeMessageEvent,
) -> bool {
    event.kind == SocketModeMessageKind::Posted
        && event
            .message
            .user
            .as_deref()
            .is_none_or(|user| Some(user) != current_user_id)
}

fn actively_reading_channel(
    window_active: bool,
    selected_channel: Option<&str>,
    channel_id: &str,
) -> bool {
    window_active && selected_channel == Some(channel_id)
}

const THREAD_PANE_MIN_FRACTION: f64 = 0.2;
const THREAD_PANE_MAX_FRACTION: f64 = 2.0 / 3.0;

fn resized_end_sidebar_fraction(
    starting_sidebar_width: f64,
    horizontal_offset: f64,
    split_width: f64,
) -> Option<f64> {
    (split_width > 0.0).then(|| {
        ((starting_sidebar_width - horizontal_offset) / split_width)
            .clamp(THREAD_PANE_MIN_FRACTION, THREAD_PANE_MAX_FRACTION)
    })
}

fn first_unread_message_ts(
    messages: &[SlackMessage],
    last_read: Option<&str>,
    unread_count: u64,
) -> Option<String> {
    let mut timestamps = messages
        .iter()
        .map(|message| message.ts.as_str())
        .filter(|ts| !ts.is_empty())
        .collect::<Vec<_>>();
    timestamps.sort_unstable();
    if let Some(last_read) = last_read.filter(|ts| !ts.trim().is_empty()) {
        return timestamps
            .into_iter()
            .find(|timestamp| *timestamp > last_read)
            .map(ToString::to_string);
    }
    if unread_count == 0 {
        return None;
    }
    let index = timestamps.len().saturating_sub(unread_count as usize);
    timestamps
        .get(index)
        .map(|timestamp| (*timestamp).to_string())
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
        media: true,
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

        imp.thread_resize_handle
            .set_cursor_from_name(Some("col-resize"));
        let initial_width = Rc::new(Cell::new(0.0));
        let drag = gtk::GestureDrag::new();
        let weak_window = self.downgrade();
        let drag_initial_width = initial_width.clone();
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(window) = weak_window.upgrade() {
                let imp = window.imp();
                let width = imp
                    .thread_resize_handle
                    .parent()
                    .map(|parent| parent.width())
                    .unwrap_or_else(|| {
                        (f64::from(imp.thread_split.width())
                            * imp.thread_split.sidebar_width_fraction())
                            as i32
                    });
                drag_initial_width.set(f64::from(width));
            }
        });
        let weak_window = self.downgrade();
        drag.connect_drag_update(move |_, offset_x, _| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let split = &window.imp().thread_split;
            if let Some(fraction) = resized_end_sidebar_fraction(
                initial_width.get(),
                offset_x,
                f64::from(split.width()),
            ) {
                split.set_sidebar_width_fraction(fraction);
            }
        });
        imp.thread_resize_handle.add_controller(drag);

        let keys = gtk::EventControllerKey::new();
        let weak_window = self.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            let offset = match key {
                gtk::gdk::Key::Left => -16.0,
                gtk::gdk::Key::Right => 16.0,
                _ => return glib::Propagation::Proceed,
            };
            let Some(window) = weak_window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let imp = window.imp();
            let split_width = f64::from(imp.thread_split.width());
            let sidebar_width = imp
                .thread_resize_handle
                .parent()
                .map(|parent| f64::from(parent.width()))
                .unwrap_or(split_width * imp.thread_split.sidebar_width_fraction());
            if let Some(fraction) = resized_end_sidebar_fraction(sidebar_width, offset, split_width)
            {
                imp.thread_split.set_sidebar_width_fraction(fraction);
            }
            glib::Propagation::Stop
        });
        imp.thread_resize_handle.add_controller(keys);

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
        thread_breakpoint.add_setter(
            &imp.thread_resize_handle.get(),
            "visible",
            Some(&false.to_value()),
        );
        self.add_breakpoint(thread_breakpoint);
    }

    fn configure_accessibility(&self) {
        let imp = self.imp();
        imp.message_entry
            .update_property(&[gtk::accessible::Property::Label("Message")]);
        imp.thread_entry
            .update_property(&[gtk::accessible::Property::Label("Reply")]);
        imp.thread_resize_handle
            .update_property(&[gtk::accessible::Property::Label("Resize thread pane")]);
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
                imp.threads_button.get().upcast::<gtk::Widget>(),
                gettext("Threads"),
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
            let mut events_since_yield = 0_usize;
            while let Some(event) = events.recv().await {
                let Some(window) = weak_window.upgrade() else {
                    return;
                };
                startup_failed |= runtime_event_is_start_failure(&event);
                window.handle_runtime_event(event);
                events_since_yield += 1;
                if events_since_yield >= UI_EVENT_BATCH_LIMIT {
                    events_since_yield = 0;
                    // Leave a real scheduling gap so GTK can process input, frame
                    // callbacks, and pending redraws before draining more events.
                    glib::timeout_future(Duration::from_millis(1)).await;
                }
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
        let thread_pane = ThreadPane::new(
            &self.imp().thread_split.get(),
            &self.imp().thread_title.get(),
            &self.imp().thread_view_box.get(),
            thread_view,
        );
        *self.imp().thread_pane_controller.borrow_mut() = Some(thread_pane);

        self.show_message_placeholder(&gettext("Select a conversation"));
        self.thread_pane().close();
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
        let image = gtk::DrawingArea::new();
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        let image_source = Rc::new(RefCell::new(None::<gdk_pixbuf::Pixbuf>));
        let draw_source = image_source.clone();
        image.set_draw_func(move |_, context, width, height| {
            let source = draw_source.borrow();
            let Some(pixbuf) = source.as_ref() else {
                return;
            };
            let source_width = pixbuf.width().max(1) as f64;
            let source_height = pixbuf.height().max(1) as f64;
            context.scale(
                width.max(1) as f64 / source_width,
                height.max(1) as f64 / source_height,
            );
            context.set_source_pixbuf(pixbuf, 0.0, 0.0);
            let _ = context.paint();
        });
        let image_canvas = gtk::CenterBox::new();
        image_canvas.set_orientation(gtk::Orientation::Vertical);
        image_canvas.set_hexpand(true);
        image_canvas.set_vexpand(true);
        image_canvas.set_center_widget(Some(&image));
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
            image,
            image_source,
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

        for property in ["width", "height"] {
            let weak_window = self.downgrade();
            viewer
                .image_scroller
                .connect_notify_local(Some(property), move |_, _| {
                    if let Some(window) = weak_window.upgrade() {
                        window.reapply_media_zoom();
                    }
                });
        }

        let close_click = gtk::GestureClick::new();
        close_click.set_button(gtk::gdk::BUTTON_PRIMARY);
        let weak_window = self.downgrade();
        close_click.connect_released(move |_, _, _, _| {
            if let Some(window) = weak_window.upgrade() {
                window.close_media_viewer();
            }
        });
        viewer.image.add_controller(close_click);

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

    fn reapply_media_zoom(&self) {
        if let Some(viewer) = self.imp().media_viewer.borrow().as_ref() {
            if viewer.content_stack.visible_child_name().as_deref() == Some("image") {
                apply_media_zoom(viewer);
            }
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
            match gdk_pixbuf::Pixbuf::from_file(&path) {
                Ok(pixbuf) => {
                    viewer.natural_size = (pixbuf.width(), pixbuf.height());
                    *viewer.image_source.borrow_mut() = Some(pixbuf);
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

        clear_stale_upload_staging();
        self.setup_window_actions();
        self.connect_close_request(|window| {
            window.flush_drafts();
            glib::Propagation::Proceed
        });
        self.connect_widget(&imp.connect_button.get(), |window| window.start_auth());
        self.connect_widget(&imp.messages_button.get(), |window| window.show_messages());
        self.connect_widget(&imp.unreads_button.get(), |window| window.show_unreads());
        self.connect_widget(&imp.threads_button.get(), |window| window.show_threads());
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

        for target in COMPOSER_TARGETS {
            self.setup_composer_emoji_completion(target);
        }

        let weak_window = self.downgrade();
        imp.browser_session_check.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.update_auth_mode_ui();
            }
        });

        let weak_window = self.downgrade();
        imp.sidebar_filter_entry.connect_search_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.schedule_sidebar_filter();
            }
        });

        let weak_window = self.downgrade();
        imp.sidebar_unread_filter_button.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.queue_ui_invalidations(UiInvalidations::SIDEBAR);
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
        self.connect_image_paste(&imp.message_entry.get(), false);
        self.connect_image_paste(&imp.thread_entry.get(), true);
        self.connect_conversation_pane_image_paste();

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
                    window.queue_ui_invalidations(UiInvalidations::SIDEBAR);
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

    fn schedule_sidebar_filter(&self) {
        let generation = self.imp().sidebar_filter_generation.get().saturating_add(1);
        self.imp().sidebar_filter_generation.set(generation);
        let weak_window = self.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(90), move || {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            if window.imp().sidebar_filter_generation.get() == generation {
                window.queue_ui_invalidations(UiInvalidations::SIDEBAR);
            }
        });
    }

    fn schedule_picker_filter(&self) {
        let generation = self.imp().picker_filter_generation.get().saturating_add(1);
        self.imp().picker_filter_generation.set(generation);
        let weak_window = self.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(90), move || {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            if window.imp().picker_filter_generation.get() == generation {
                window.queue_ui_invalidations(UiInvalidations::PICKER);
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

    fn complete_upload_draft(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        submitted: Option<&str>,
    ) {
        let Some(submitted) = submitted else {
            return;
        };
        let Some(key) = self.draft_key(channel_id, thread_ts) else {
            return;
        };
        let current_target_matches = self.visible_channel_id().as_deref() == Some(channel_id)
            && thread_ts
                .is_none_or(|thread_ts| self.selected_thread_ts().as_deref() == Some(thread_ts));
        let current_text = current_target_matches.then(|| {
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
        if !submitted_draft_matches(current_text.as_deref(), stored_text.as_deref(), submitted) {
            return;
        }

        if stored_text.is_some_and(|text| text.trim() == submitted)
            && self.imp().drafts.borrow_mut().remove(&key)
        {
            self.persist_drafts();
        }
        if current_text.is_some() {
            if thread_ts.is_some() {
                set_text_view_text(&self.imp().thread_entry, "");
            } else {
                set_text_view_text(&self.imp().message_entry, "");
            }
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

    fn composer_text_view(&self, target: ComposerTarget) -> gtk::TextView {
        match target {
            ComposerTarget::Message => self.imp().message_entry.get(),
            ComposerTarget::Thread => self.imp().thread_entry.get(),
        }
    }

    fn setup_composer_emoji_completion(&self, target: ComposerTarget) {
        let text_view = self.composer_text_view(target);
        let popover = gtk::Popover::new();
        popover.set_parent(&text_view);
        // Autohide popovers take focus when they open, which immediately trips
        // the composer's focus-loss dismissal and prevents keyboard completion.
        // We already dismiss explicitly when focus leaves the composer.
        popover.set_autohide(false);
        popover.set_has_arrow(true);
        popover.set_position(gtk::PositionType::Bottom);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.update_property(&[gtk::accessible::Property::Label("Emoji suggestions")]);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_min_content_width(280);
        scroller.set_max_content_height(320);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&list));
        popover.set_child(Some(&scroller));

        let completion = ComposerEmojiCompletion {
            popover: popover.clone(),
            list: list.clone(),
            entries: Vec::new(),
            token: None,
        };
        match target {
            ComposerTarget::Message => {
                *self.imp().message_emoji_completion.borrow_mut() = Some(completion)
            }
            ComposerTarget::Thread => {
                *self.imp().thread_emoji_completion.borrow_mut() = Some(completion)
            }
        }

        let weak_window = self.downgrade();
        list.connect_row_activated(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.accept_composer_emoji_completion(target);
            }
        });

        let weak_window = self.downgrade();
        text_view.buffer().connect_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.refresh_composer_emoji_completion(target);
            }
        });

        let weak_window = self.downgrade();
        text_view.buffer().connect_mark_set(move |_, _, mark| {
            if mark.name().as_deref() == Some("insert") {
                if let Some(window) = weak_window.upgrade() {
                    window.refresh_composer_emoji_completion(target);
                }
            }
        });

        let weak_window = self.downgrade();
        text_view.connect_has_focus_notify(move |text_view| {
            if text_view.has_focus() {
                return;
            }
            let weak_window = weak_window.clone();
            glib::idle_add_local_once(move || {
                if let Some(window) = weak_window.upgrade() {
                    if !window.composer_text_view(target).has_focus() {
                        window.dismiss_composer_emoji_completion(target);
                    }
                }
            });
        });

        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_window = self.downgrade();
        controller.connect_key_pressed(move |_, key, _, state| {
            weak_window
                .upgrade()
                .map_or(glib::Propagation::Proceed, |window| {
                    window.handle_composer_emoji_completion_key(target, key, state)
                })
        });
        text_view.add_controller(controller);
    }

    fn refresh_composer_emoji_completion(&self, target: ComposerTarget) {
        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let text = text_view_text(&text_view);
        let caret = buffer.cursor_position().max(0) as usize;
        let token = emoji_token_at_caret(&text, caret);
        let entries = token.as_ref().map_or_else(Vec::new, |token| {
            let custom_emojis = self.imp().custom_emojis.borrow();
            let catalog = EmojiCatalog::new(&custom_emojis);
            EmojiPickerModel::new(catalog.entries())
                .search(&token.query)
                .into_iter()
                .take(10)
                .collect::<Vec<_>>()
        });

        let mut completion_ref = match target {
            ComposerTarget::Message => self.imp().message_emoji_completion.borrow_mut(),
            ComposerTarget::Thread => self.imp().thread_emoji_completion.borrow_mut(),
        };
        let Some(completion) = completion_ref.as_mut() else {
            return;
        };
        completion.token = token;
        completion.entries = entries;

        while let Some(child) = completion.list.first_child() {
            completion.list.remove(&child);
        }
        if completion.entries.is_empty() {
            completion.popover.popdown();
            return;
        }

        for entry in &completion.entries {
            let row = gtk::ListBoxRow::new();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            content.set_margin_top(6);
            content.set_margin_bottom(6);
            content.set_margin_start(8);
            content.set_margin_end(8);
            let preview = composer_emoji_preview(entry);
            let label = gtk::Label::new(Some(&format!(":{}:  {}", entry.name, entry.label)));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            content.append(&preview);
            content.append(&label);
            row.set_child(Some(&content));
            row.update_property(&[gtk::accessible::Property::Label(
                &emoji_picker_accessible_label(entry),
            )]);
            completion.list.append(&row);
        }
        completion
            .list
            .select_row(completion.list.row_at_index(0).as_ref());

        let insert = buffer.iter_at_offset(buffer.cursor_position());
        completion
            .popover
            .set_pointing_to(Some(&text_view.iter_location(&insert)));
        completion.popover.popup();
    }

    fn dismiss_composer_emoji_completion(&self, target: ComposerTarget) {
        let mut completion_ref = match target {
            ComposerTarget::Message => self.imp().message_emoji_completion.borrow_mut(),
            ComposerTarget::Thread => self.imp().thread_emoji_completion.borrow_mut(),
        };
        if let Some(completion) = completion_ref.as_mut() {
            completion.token = None;
            completion.entries.clear();
            completion.popover.popdown();
        }
    }

    fn move_composer_emoji_selection(&self, target: ComposerTarget, movement: EmojiPickerMove) {
        let completion_ref = match target {
            ComposerTarget::Message => self.imp().message_emoji_completion.borrow(),
            ComposerTarget::Thread => self.imp().thread_emoji_completion.borrow(),
        };
        let Some(completion) = completion_ref.as_ref() else {
            return;
        };
        let current = completion
            .list
            .selected_row()
            .map(|row| row.index().max(0) as usize);
        if let Some(next) = move_emoji_picker_selection(current, completion.entries.len(), movement)
        {
            completion
                .list
                .select_row(completion.list.row_at_index(next as i32).as_ref());
        }
    }

    fn accept_composer_emoji_completion(&self, target: ComposerTarget) {
        let selection = {
            let completion_ref = match target {
                ComposerTarget::Message => self.imp().message_emoji_completion.borrow(),
                ComposerTarget::Thread => self.imp().thread_emoji_completion.borrow(),
            };
            let Some(completion) = completion_ref.as_ref() else {
                return;
            };
            let Some(token) = completion.token.clone() else {
                return;
            };
            let index = completion
                .list
                .selected_row()
                .map_or(0, |row| row.index().max(0) as usize);
            let Some(entry) = completion.entries.get(index) else {
                return;
            };
            (token, entry.name.clone())
        };

        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let (updated, caret) =
            replace_emoji_token(&text_view_text(&text_view), &selection.0, &selection.1);
        let replacement = updated
            .chars()
            .skip(selection.0.start)
            .take(caret.saturating_sub(selection.0.start))
            .collect::<String>();
        let mut start = buffer.iter_at_offset(selection.0.start as i32);
        let mut end = buffer.iter_at_offset(selection.0.end as i32);
        buffer.begin_user_action();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &replacement);
        buffer.place_cursor(&buffer.iter_at_offset(caret as i32));
        buffer.end_user_action();
        self.dismiss_composer_emoji_completion(target);
        text_view.grab_focus();
    }

    fn handle_composer_emoji_completion_key(
        &self,
        target: ComposerTarget,
        key: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        let is_open = {
            let completion_ref = match target {
                ComposerTarget::Message => self.imp().message_emoji_completion.borrow(),
                ComposerTarget::Thread => self.imp().thread_emoji_completion.borrow(),
            };
            completion_ref
                .as_ref()
                .is_some_and(|completion| completion.popover.is_visible())
        };
        if !is_open {
            return glib::Propagation::Proceed;
        }

        match emoji_completion_key_action(key, state) {
            EmojiCompletionKeyAction::Previous => {
                self.move_composer_emoji_selection(target, EmojiPickerMove::Previous)
            }
            EmojiCompletionKeyAction::Next => {
                self.move_composer_emoji_selection(target, EmojiPickerMove::Next)
            }
            EmojiCompletionKeyAction::Accept => self.accept_composer_emoji_completion(target),
            EmojiCompletionKeyAction::Dismiss => self.dismiss_composer_emoji_completion(target),
            EmojiCompletionKeyAction::Ignore => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn handle_runtime_event(&self, event: RuntimeEvent) {
        let started = Instant::now();
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
            RuntimeEventKind::WorkspaceLifecycle(event) => {
                self.apply_workspace_lifecycle(event);
            }
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
            RuntimeEventKind::RuntimeStartFailed(error) => self.show_session_error(&error.message),
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
                    self.show_conversation_load_error(&error.message);
                }
            }
            RuntimeEventKind::ConversationChannelsDiscovered(channels) => {
                *self.imp().discovered_channels.borrow_mut() = channels;
                self.refresh_open_conversation_picker();
            }
            RuntimeEventKind::ConversationPeopleDiscovered(users) => {
                let names = users
                    .iter()
                    .filter_map(|user| Some((user.id.clone()?, user.display_name()?)))
                    .collect::<HashMap<_, _>>();
                self.populate_user_names(names);
                self.replace_user_statuses(
                    users
                        .iter()
                        .filter_map(|user| Some((user.id.clone()?, user.status()?)))
                        .collect(),
                );
                *self.imp().discovered_users.borrow_mut() = users;
                self.refresh_open_conversation_picker();
            }
            RuntimeEventKind::ConversationOpened(conversation) => {
                let channel_id = conversation.id.clone();
                let imp = self.imp();
                let title = conversation.display_name_with_users(
                    &imp.user_names.borrow(),
                    imp.current_user_id.borrow().as_deref(),
                );
                imp.workspace
                    .conversations
                    .borrow_mut()
                    .upsert_metadata(conversation);
                imp.pending_opened_conversation_ids
                    .borrow_mut()
                    .insert(channel_id.clone());
                self.sync_conversations_from_catalog();
                self.select_conversation(&channel_id, &title);
            }
            RuntimeEventKind::ConversationLeft { channel_id } => {
                self.apply_conversation_left(&channel_id);
            }
            RuntimeEventKind::ConversationsPatched {
                conversations,
                unread_states,
            } => {
                let mut catalog = self.imp().workspace.conversations.borrow_mut();
                for conversation in conversations {
                    catalog.upsert_metadata(conversation);
                }
                for (channel_id, unread_state, server_last_read) in unread_states {
                    let newer_local_read = self
                        .imp()
                        .local_read_ts_by_channel
                        .borrow()
                        .get(&channel_id)
                        .is_some_and(|local| {
                            server_last_read
                                .as_deref()
                                .is_none_or(|server| local.as_str() > server)
                        });
                    if !newer_local_read {
                        catalog.apply_realtime_unread(&channel_id, unread_state);
                    }
                }
                drop(catalog);
                self.sync_conversations_from_catalog();
            }
            RuntimeEventKind::ConversationUnreadUpdated {
                channel_id,
                unread_state,
            } => self.apply_conversation_unread_state(&channel_id, unread_state),
            RuntimeEventKind::ConversationMarkedRead { channel_id, ts } => {
                self.imp()
                    .local_read_ts_by_channel
                    .borrow_mut()
                    .insert(channel_id.clone(), ts.clone());
                self.advance_conversation_read_cursor(&channel_id, &ts);
                self.render_conversations();
                if self.current_main_view() == MainMessageView::Unreads {
                    self.populate_unreads(self.unread_items());
                }
            }
            RuntimeEventKind::ConversationNotificationCandidate {
                channel_id,
                messages,
            } => self.notify_if_new_messages(
                &channel_id,
                &messages,
                MessageNotificationDelivery::Snapshot,
            ),
            RuntimeEventKind::ThreadCatalogLoaded(records) => {
                *self.imp().workspace.threads.borrow_mut() = ThreadCatalog::from_records(records);
                if self.current_main_view() == MainMessageView::Threads {
                    self.populate_threads();
                } else if self.current_main_view() == MainMessageView::Unreads {
                    self.populate_unreads(self.unread_items());
                }
            }
            RuntimeEventKind::HistoryLoaded {
                channel_id,
                messages,
                has_more,
                next_cursor,
                append_older,
                cached,
            } => {
                let outcome = self.imp().workspace.view.borrow_mut().apply_history(
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
                        .workspace
                        .view
                        .borrow()
                        .snapshot()
                        .channel_messages;
                    if outcome.notify_new_messages {
                        self.notify_if_new_messages(
                            &channel_id,
                            &rendered_messages,
                            MessageNotificationDelivery::Snapshot,
                        );
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
                let outcome = self.imp().workspace.view.borrow_mut().apply_thread(
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
                        .workspace
                        .view
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
                    .workspace
                    .view
                    .borrow_mut()
                    .apply_message_context(&location, messages);
                if visible {
                    if let Some(thread_ts) = location.thread_ts() {
                        let messages = self
                            .imp()
                            .workspace
                            .view
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
                            .workspace
                            .view
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
                    .workspace
                    .view
                    .borrow_mut()
                    .apply_search_results(results);
                if visible {
                    let results = self.imp().workspace.view.borrow().search_results().to_vec();
                    self.populate_search_results(results);
                }
            }
            RuntimeEventKind::FilesLoaded(files) => {
                let visible = self.imp().workspace.view.borrow_mut().apply_files(files);
                if visible {
                    let files = self.imp().workspace.view.borrow().files().to_vec();
                    self.populate_files(files);
                }
            }
            RuntimeEventKind::SavedItemsLoaded(items) => {
                let visible = self.imp().workspace.view.borrow_mut().apply_saved(items);
                if visible {
                    let items = self.imp().workspace.view.borrow().saved_items().to_vec();
                    self.populate_saved_items(items);
                }
            }
            RuntimeEventKind::SocketModeEvent(event) => self.handle_socket_mode_event(event),
            RuntimeEventKind::UserLoaded {
                user_id,
                display_name,
                full_name,
                status,
            } => {
                self.populate_user_names(HashMap::from([(user_id.clone(), display_name)]));
                if let Some(full_name) = full_name {
                    self.populate_user_full_names(HashMap::from([(user_id.clone(), full_name)]));
                }
                if let Some(status) = status {
                    self.populate_user_statuses(HashMap::from([(user_id, status)]));
                }
            }
            RuntimeEventKind::UserProfileLoaded(user) => {
                let user_id = user.id.clone().unwrap_or_default();
                let expected = self.imp().pending_profile_user_id.borrow().clone();
                if expected.as_deref() == Some(user_id.as_str()) {
                    self.imp().pending_profile_user_id.borrow_mut().take();
                    self.imp()
                        .message_title
                        .set_title(&user.display_name().unwrap_or_else(|| gettext("Profile")));
                    let context = self.message_html_context(None);
                    self.load_message_html(&message_html::user_profile_document(&user, &context));
                }
            }
            RuntimeEventKind::UserNamesLoaded(user_names) => self.populate_user_names(user_names),
            RuntimeEventKind::UserFullNamesLoaded(names) => self.populate_user_full_names(names),
            RuntimeEventKind::UserSearchAliasesLoaded(aliases) => {
                *self.imp().user_search_aliases.borrow_mut() = aliases;
                self.queue_ui_invalidations(UiInvalidations::SIDEBAR | UiInvalidations::PICKER);
            }
            RuntimeEventKind::UserStatusesLoaded(statuses) => {
                self.replace_user_statuses(statuses);
            }
            RuntimeEventKind::UserGroupsLoaded { names, members } => {
                self.populate_user_groups(names, members);
            }
            RuntimeEventKind::EmojiCatalogLoaded(emojis) => {
                *self.imp().custom_emojis.borrow_mut() = emojis;
                self.queue_ui_invalidations(
                    UiInvalidations::SIDEBAR
                        | UiInvalidations::PICKER
                        | UiInvalidations::TITLE
                        | UiInvalidations::MAIN
                        | UiInvalidations::THREAD,
                );
                for target in COMPOSER_TARGETS {
                    self.refresh_composer_emoji_completion(target);
                }
            }
            RuntimeEventKind::ImageAssetLoaded { key, data_uri } => {
                crate::debug::log(
                    "ui",
                    &format!("ImageAssetLoaded key={}", crate::debug::url_for_log(&key)),
                );
                let imp = self.imp();
                imp.pending_image_assets.borrow_mut().remove(&key);
                imp.failed_image_assets.borrow_mut().remove(&key);
                imp.image_assets
                    .borrow_mut()
                    .insert(key.clone(), data_uri.clone());
                self.patch_image_asset(&key, Some(data_uri));
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
            RuntimeEventKind::AttachmentDownloadProgress { fraction, label } => {
                self.set_status(&format!("{label} ({:.0}%)", fraction * 100.0));
            }
            RuntimeEventKind::AttachmentDownloaded { url: _, name, path } => {
                match open::that(&path) {
                    Ok(()) => self.set_status(&format!("Opened {name}")),
                    Err(error) => self.set_status(&format!("Could not open {name}: {error}")),
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
                imp.thread_send_button.set_sensitive(true);
                imp.upload_progress.set_fraction(1.0);
                imp.upload_progress.set_text(Some("Upload complete"));
                self.set_status(&format!("Uploaded {name}"));
                let upload_target = match &meta.context.target {
                    RuntimeTarget::Upload {
                        channel_id,
                        thread_ts,
                    } => Some((channel_id.as_str(), thread_ts.as_deref())),
                    _ => None,
                };
                if let Some((channel_id, thread_ts)) = upload_target {
                    let submitted = self.draft_key(channel_id, thread_ts).and_then(|key| {
                        imp.pending_upload_drafts
                            .borrow_mut()
                            .remove(&key)
                            .flatten()
                    });
                    self.complete_upload_draft(channel_id, thread_ts, submitted.as_deref());
                    self.reload_after_message(channel_id, thread_ts);
                }
            }
        }
        log_performance(started, |elapsed_ms| {
            format!(
                "runtime_event operation={:?} elapsed_ms={:.2}",
                meta.context.operation, elapsed_ms
            )
        });
    }

    fn queue_ui_invalidations(&self, invalidations: UiInvalidations) {
        let mut pending = self.imp().pending_ui_invalidations.get();
        let should_schedule = pending.insert(invalidations);
        self.imp().pending_ui_invalidations.set(pending);
        if !should_schedule {
            return;
        }

        let weak_window = self.downgrade();
        self.add_tick_callback(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.flush_ui_invalidations();
            }
            glib::ControlFlow::Break
        });
    }

    fn flush_ui_invalidations(&self) {
        let mut pending = self.imp().pending_ui_invalidations.get();
        let invalidations = pending.take();
        self.imp().pending_ui_invalidations.set(pending);
        let started = Instant::now();

        if invalidations.contains(UiInvalidations::SIDEBAR) {
            self.render_conversations();
        }
        if invalidations.contains(UiInvalidations::PICKER) {
            self.refresh_open_conversation_picker();
        }
        if invalidations.contains(UiInvalidations::TITLE) {
            self.refresh_current_conversation_title();
        }
        if invalidations.contains(UiInvalidations::MAIN) {
            self.rerender_current_main_messages();
        }
        if invalidations.contains(UiInvalidations::THREAD) {
            self.rerender_current_thread();
        }

        log_performance(started, |elapsed_ms| {
            format!(
                "ui_invalidation_flush flags={:#04x} elapsed_ms={:.2}",
                invalidations.0, elapsed_ms
            )
        });
    }

    fn apply_timeline_patch(
        &self,
        surface: TimelineSurface,
        patch: TimelineDomPatch,
        fallback: UiInvalidations,
    ) {
        let web_view = match surface {
            TimelineSurface::Main => self.imp().message_view.borrow().clone(),
            TimelineSurface::Thread => Some(self.thread_pane().web_view()),
        };
        let Some(web_view) = web_view else {
            self.queue_ui_invalidations(fallback);
            return;
        };
        if web_view.is_loading() {
            self.queue_ui_invalidations(fallback);
            return;
        }

        let script = message_html::timeline_dom_patch_call(&patch);
        let weak_window = self.downgrade();
        web_view.evaluate_javascript(
            &script,
            None,
            None,
            None::<&gio::Cancellable>,
            move |result| {
                let applied = result.is_ok_and(|value| value.to_boolean());
                if !applied {
                    if let Some(window) = weak_window.upgrade() {
                        window.queue_ui_invalidations(fallback);
                    }
                }
            },
        );
    }

    fn apply_realtime_message_patch(
        &self,
        surface: TimelineSurface,
        channel_id: &str,
        message: &SlackMessage,
        kind: RealtimeMessageKind,
        unread_start: bool,
        thread_ts: Option<&str>,
        fallback: UiInvalidations,
    ) {
        let patch = match kind {
            RealtimeMessageKind::Posted => {
                let mut context = self.message_patch_context(thread_ts, message);
                if unread_start {
                    context.first_unread_ts = Some(message.ts.clone());
                }
                message_html::insert_message_patch(
                    channel_id,
                    message,
                    &context,
                    TimelineInsertPosition::Append,
                )
            }
            RealtimeMessageKind::Changed => message_html::replace_message_patch(
                channel_id,
                message,
                &self.message_patch_context(thread_ts, message),
            ),
            // Slack retains a tombstone for deleted messages. Replacing the existing
            // article keeps the incremental path consistent with a complete render.
            RealtimeMessageKind::Deleted => message_html::replace_message_patch(
                channel_id,
                message,
                &self.message_patch_context(thread_ts, message),
            ),
        };
        self.apply_timeline_patch(surface, patch, fallback);
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
            self.imp().workspace.view.borrow_mut().show_placeholder();
            self.imp().message_title.set_title(&title);
            self.show_message_placeholder(&title);
            self.render_closed_thread();
            self.render_conversations();
        }
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_unreads(&self) {
        self.flush_current_drafts();
        self.imp().workspace.view.borrow_mut().show_unreads();
        self.render_closed_thread();
        let items = self.unread_items();
        self.populate_unreads(items);
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_threads(&self) {
        self.flush_current_drafts();
        self.imp().workspace.view.borrow_mut().show_threads();
        self.render_closed_thread();
        self.populate_threads();
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_files(&self) {
        self.flush_current_drafts();
        let title = gettext("Files");
        self.imp().workspace.view.borrow_mut().start_files();
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
        self.imp().workspace.view.borrow_mut().start_saved();
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
        self.imp().workspace.view.borrow_mut().start_search();
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
        if self.thread_pane().is_open() {
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
        let Some(upload_key) = self.draft_key(&channel_id, None) else {
            self.set_status("No Slack workspace is active");
            return;
        };
        if self
            .imp()
            .pending_upload_drafts
            .borrow()
            .contains_key(&upload_key)
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
                        let initial_comment =
                            (!initial_comment.is_empty()).then(|| initial_comment.clone());
                        window.begin_file_upload(&channel_id, None, path, initial_comment, false);
                    }
                }
            }
        });
    }

    fn begin_file_upload(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        path: PathBuf,
        initial_comment: Option<String>,
        remove_after_upload: bool,
    ) {
        if self.imp().runtime.borrow().is_none() {
            self.set_status("No Slack workspace is active");
            if remove_after_upload {
                let _ = std::fs::remove_file(path);
            }
            return;
        }
        let Some(key) = self.draft_key(channel_id, thread_ts) else {
            self.set_status("No Slack workspace is active");
            if remove_after_upload {
                let _ = std::fs::remove_file(path);
            }
            return;
        };
        self.flush_current_drafts();
        if !record_upload_submission(
            &mut self.imp().pending_upload_drafts.borrow_mut(),
            key,
            initial_comment.clone(),
        ) {
            self.set_status(&gettext("A file is already being uploaded here."));
            if remove_after_upload {
                let _ = std::fs::remove_file(path);
            }
            return;
        }

        let imp = self.imp();
        if thread_ts.is_some() {
            imp.thread_send_button.set_sensitive(false);
        } else {
            imp.upload_button.set_sensitive(false);
        }
        imp.upload_progress.set_visible(true);
        imp.upload_progress.set_fraction(0.0);
        imp.upload_progress.set_text(Some("Starting upload"));
        self.send_command(RuntimeCommand::UploadFile {
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.map(ToString::to_string),
            path,
            initial_comment,
            remove_after_upload,
        });
    }

    fn connect_image_paste(&self, text_view: &gtk::TextView, thread: bool) {
        let weak_window = self.downgrade();
        text_view.connect_paste_clipboard(move |text_view| {
            let clipboard = text_view.display().clipboard();
            if !clipboard_formats_include_image(&clipboard.formats()) {
                return;
            }
            text_view.stop_signal_emission_by_name("paste-clipboard");

            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let Some(channel_id) = window.visible_channel_id() else {
                window.set_status("Select a conversation before pasting an image");
                return;
            };
            let thread_ts = if thread {
                let Some(thread_ts) = window.selected_thread_ts() else {
                    window.set_status("Open a thread before pasting an image here");
                    return;
                };
                Some(thread_ts)
            } else {
                None
            };
            let initial_comment = text_view_text(text_view).trim().to_string();
            let initial_comment = (!initial_comment.is_empty()).then_some(initial_comment);
            window.read_clipboard_image_for_upload(
                clipboard,
                &channel_id,
                thread_ts.as_deref(),
                initial_comment,
            );
        });
    }

    fn connect_conversation_pane_image_paste(&self) {
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_window = self.downgrade();
        controller.connect_key_pressed(move |_, key, _, state| {
            let Some(window) = weak_window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let clipboard = window.display().clipboard();
            let Some(target) = window.conversation_pane_paste_target(
                clipboard_formats_include_image(&clipboard.formats()),
                key,
                state,
            ) else {
                return glib::Propagation::Proceed;
            };
            let Some(channel_id) = window.visible_channel_id() else {
                window.set_status("Select a conversation before pasting an image");
                return glib::Propagation::Stop;
            };
            let thread_ts = match target {
                ComposerTarget::Message => None,
                ComposerTarget::Thread => {
                    let Some(thread_ts) = window.selected_thread_ts() else {
                        window.set_status("Open a thread before pasting an image here");
                        return glib::Propagation::Stop;
                    };
                    Some(thread_ts)
                }
            };
            window.read_clipboard_image_for_upload(
                clipboard,
                &channel_id,
                thread_ts.as_deref(),
                None,
            );
            glib::Propagation::Stop
        });
        self.add_controller(controller);
    }

    fn conversation_pane_paste_target(
        &self,
        clipboard_has_image: bool,
        key: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> Option<ComposerTarget> {
        let focus = self.focus()?;
        let imp = self.imp();
        let is_within = |widget: &gtk::Widget| focus == *widget || focus.is_ancestor(widget);
        let main_entry = imp.message_entry.get().upcast::<gtk::Widget>();
        let thread_entry = imp.thread_entry.get().upcast::<gtk::Widget>();
        let focus_kind = if is_within(&main_entry) || is_within(&thread_entry) {
            ConversationPanePasteFocus::Composer
        } else if focus.is::<gtk::Editable>() || focus.is::<gtk::TextView>() {
            ConversationPanePasteFocus::TextInput
        } else if is_within(&imp.thread_pane.get().upcast::<gtk::Widget>()) {
            ConversationPanePasteFocus::ThreadPane
        } else if is_within(&imp.message_pane.get().upcast::<gtk::Widget>()) {
            ConversationPanePasteFocus::MainPane
        } else {
            ConversationPanePasteFocus::Outside
        };
        conversation_pane_image_paste_target(focus_kind, clipboard_has_image, key, state)
    }

    fn read_clipboard_image_for_upload(
        &self,
        clipboard: gtk::gdk::Clipboard,
        channel_id: &str,
        thread_ts: Option<&str>,
        initial_comment: Option<String>,
    ) {
        let channel_id = channel_id.to_string();
        let thread_ts = thread_ts.map(ToString::to_string);
        let weak_window = self.downgrade();
        clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let texture = match result {
                Ok(Some(texture)) => texture,
                Ok(None) => {
                    window.set_status("The clipboard image could not be read");
                    return;
                }
                Err(error) => {
                    window.set_status(&format!("Could not read clipboard image: {error}"));
                    return;
                }
            };

            let directory = config::upload_staging_dir();
            if let Err(error) = std::fs::create_dir_all(&directory) {
                window.set_status(&format!("Could not prepare screenshot upload: {error}"));
                return;
            }
            let path = directory.join(screenshot_filename());
            if let Err(error) = texture.save_to_png(&path) {
                let _ = std::fs::remove_file(&path);
                window.set_status(&format!("Could not encode clipboard image: {error}"));
                return;
            }
            window.begin_file_upload(
                &channel_id,
                thread_ts.as_deref(),
                path,
                initial_comment,
                true,
            );
        });
    }

    fn close_thread(&self) {
        self.flush_current_drafts();
        self.imp().workspace.view.borrow_mut().close_thread();
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
            .workspace
            .view
            .borrow_mut()
            .open_thread(channel_id, ts);
        self.restore_thread_draft(channel_id, ts);
        match outcome {
            ThreadOpenOutcome::RenderCurrent => {
                let messages = self
                    .imp()
                    .workspace
                    .view
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
                self.thread_pane()
                    .show_placeholder(&gettext("Loading thread"));
                self.send_command(RuntimeCommand::LoadThread {
                    channel_id: channel_id.to_string(),
                    ts: ts.to_string(),
                });
            }
            ThreadOpenOutcome::AwaitFresh => {
                self.set_status(&gettext("Loading thread"));
                self.thread_pane()
                    .show_placeholder(&gettext("Loading thread"));
            }
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
            .workspace
            .view
            .borrow_mut()
            .focus_message(&location)
        {
            return;
        }
        self.set_status(&gettext("Loading message context"));
        self.send_command(RuntimeCommand::LoadMessageContext(location));
    }

    fn render_closed_thread(&self) {
        set_text_view_text(&self.imp().thread_entry, "");
        self.thread_pane().close();
    }

    fn handle_message_view_uri(&self, uri: &str) -> bool {
        let Ok(url) = url::Url::parse(uri) else {
            return false;
        };

        match url.scheme() {
            "conduit" => self.handle_message_action_url(&url),
            "http" | "https" => {
                let workspace_url = self.imp().workspace_url.borrow().clone();
                if let Some(location) = slack_message_location(uri, workspace_url.as_deref()) {
                    self.open_message_context(location);
                } else {
                    self.open_external_link(uri);
                }
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
            Some("mark-read") => {
                let Some(channel_id) = query_param(url, "channel") else {
                    return true;
                };
                let Some(ts) = query_param(url, "ts") else {
                    return true;
                };
                if let Some(thread_ts) = query_param(url, "thread_ts") {
                    if self.visible_channel_id().as_deref() == Some(channel_id.as_str())
                        && self.selected_thread_ts().as_deref() == Some(thread_ts.as_str())
                    {
                        self.send_command(RuntimeCommand::MarkThreadRead {
                            channel_id,
                            thread_ts,
                            ts,
                        });
                    }
                } else if self.visible_channel_id().as_deref() == Some(channel_id.as_str()) {
                    self.send_command(RuntimeCommand::MarkConversationRead { channel_id, ts });
                }
                true
            }
            Some("user-message") => {
                if let Some(user_id) = query_param(url, "user") {
                    self.send_command(RuntimeCommand::OpenDirectMessage { user_id });
                }
                true
            }
            Some("user-profile") => {
                if let Some(user_id) = query_param(url, "user") {
                    *self.imp().pending_profile_user_id.borrow_mut() = Some(user_id.clone());
                    self.imp().message_title.set_title(&gettext("Profile"));
                    self.load_message_html(&message_html::placeholder_document(
                        &gettext("Profile"),
                        &gettext("Loading profile"),
                    ));
                    self.send_command(RuntimeCommand::LoadUserProfile { user_id });
                }
                true
            }
            Some("profile-close") => {
                self.imp().pending_profile_user_id.borrow_mut().take();
                self.queue_ui_invalidations(UiInvalidations::MAIN);
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
                    let should_load = {
                        let mut state = self.imp().workspace.view.borrow_mut();
                        state.visible_channel_id() == Some(channel_id.as_str())
                            && state.selected_thread_ts() == Some(ts.as_str())
                            && state.thread_cursor() == Some(cursor.as_str())
                            && state.begin_thread_history_request()
                    };
                    if should_load {
                        self.set_status("Loading more replies");
                        self.send_command(RuntimeCommand::LoadOlderThread {
                            channel_id,
                            ts,
                            cursor,
                        });
                    }
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
            Some("attachment") => {
                let Some(attachment_url) = query_param(url, "url").filter(|url| {
                    url::Url::parse(url)
                        .ok()
                        .is_some_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
                }) else {
                    self.set_status("Invalid attachment link");
                    return true;
                };
                let name = query_param(url, "name")
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "Attachment".to_string());
                self.set_status(&format!("Downloading {name}"));
                self.send_command(RuntimeCommand::DownloadAttachment {
                    url: attachment_url,
                    name,
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
            .workspace
            .view
            .borrow()
            .find_message(channel_id, ts)
    }

    fn note_thread_reply_posted(&self, channel_id: &str, thread_ts: &str) {
        let should_render = {
            let mut state = self.imp().workspace.view.borrow_mut();
            state.increment_thread_reply(channel_id, thread_ts)
                && state.visible_channel_id() == Some(channel_id)
        };
        if should_render {
            let messages = self
                .imp()
                .workspace
                .view
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
                let mut state = self.imp().workspace.view.borrow_mut();
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
        self.apply_workspace_lifecycle(WorkspaceLifecycleEvent::ConnectRequested);
        self.imp().status_label.set_label(status);
    }

    fn show_login(&self, status: &str) {
        let imp = self.imp();
        self.reset_workspace_state();
        imp.content_stack.set_visible_child_name("connect");
        self.render_workspace_lifecycle();
        if !status.is_empty() {
            imp.connection_label.set_label(status);
        }
    }

    pub(crate) fn show_connect_requested(&self) {
        self.send_session_command(RuntimeCommand::Disconnect);
        self.imp().connect_requested.set(true);
        self.imp()
            .workspace
            .transition_lifecycle(WorkspaceLifecycleEvent::SignedOut);
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
        imp.workspace.reset();
        *imp.current_user_id.borrow_mut() = None;
        *imp.workspace_id.borrow_mut() = None;
        imp.workspace_ready.set(false);
        imp.latest_message_ts_by_channel.borrow_mut().clear();
        imp.local_read_ts_by_channel.borrow_mut().clear();
        imp.seen_realtime_messages.borrow_mut().clear();
        imp.pending_opened_conversation_ids.borrow_mut().clear();
        imp.pending_sent_drafts.borrow_mut().clear();
        imp.pending_upload_drafts.borrow_mut().clear();
        imp.discovered_channels.borrow_mut().clear();
        imp.discovered_users.borrow_mut().clear();
        imp.sidebar_row_actions.borrow_mut().clear();
        imp.user_names.borrow_mut().clear();
        imp.user_full_names.borrow_mut().clear();
        imp.user_search_aliases.borrow_mut().clear();
        imp.user_statuses.borrow_mut().clear();
        imp.status_expiry_generation
            .set(imp.status_expiry_generation.get().saturating_add(1));
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
        imp.custom_emojis.borrow_mut().clear();
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
        imp.workspace_title_label.set_title(&gettext("Workspace"));
        imp.workspace_status_label.set_label("");
        imp.message_status_label.set_label("");
        imp.workspace_split.set_show_content(false);
        self.thread_pane().close();
        self.sync_workspace_chrome();
        self.clear_list(&imp.conversation_list);
        imp.sidebar_items.borrow_mut().clear();
        imp.sidebar_rows.borrow_mut().clear();
        imp.sidebar_row_actions.borrow_mut().clear();
        self.show_message_placeholder(&gettext("Select a conversation"));
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
        self.set_status("");
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
        imp.message_status_label.set_label(status);
    }

    fn restore_workspace_status(&self) {
        self.imp().message_status_label.set_label("");
        self.render_workspace_lifecycle();
    }

    fn apply_workspace_lifecycle(&self, event: WorkspaceLifecycleEvent) {
        self.imp().workspace.transition_lifecycle(event);
        self.render_workspace_lifecycle();
    }

    fn render_workspace_lifecycle(&self) {
        let imp = self.imp();
        let presentation = workspace_lifecycle_presentation(
            imp.workspace.lifecycle(),
            imp.workspace_id.borrow().is_some(),
        );
        let status = gettext(presentation.status);
        imp.connection_label.set_label(&status);
        imp.workspace_status_label.set_label(&status);
        imp.connect_button
            .set_sensitive(imp.workspace.lifecycle() != WorkspaceLifecycle::Connecting);
        match presentation.surface {
            WorkspaceLifecycleSurface::Connect => {
                imp.content_stack.set_visible_child_name("connect")
            }
            WorkspaceLifecycleSurface::Loading => {
                imp.status_label.set_label(&status);
                imp.content_stack.set_visible_child_name("loading");
            }
            WorkspaceLifecycleSurface::Workspace => {
                imp.content_stack.set_visible_child_name("workspace")
            }
        }
    }

    fn start_sidebar_loading(&self) {
        let imp = self.imp();
        if !imp.sidebar_loading.replace(true) {
            *imp.sidebar_error.borrow_mut() = None;
            if imp.workspace.conversations.borrow().is_empty() {
                self.render_conversations();
            }
        }
    }

    fn handle_runtime_error(&self, context: &OperationContext, failure: &RuntimeFailure) {
        let error = failure.message.as_str();
        match runtime_failure_recovery_for_failure(context, failure) {
            RuntimeFailureRecovery::Session => self.show_session_error(error),
            RuntimeFailureRecovery::Sidebar => self.show_conversation_load_error(error),
            RuntimeFailureRecovery::History(channel_id) => {
                let outcome = self
                    .imp()
                    .workspace
                    .view
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
                    .workspace
                    .view
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
                let outcome = self.imp().workspace.view.borrow_mut().fail_search();
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::SearchResults, error);
                    }
                }
            }
            RuntimeFailureRecovery::Files => {
                let outcome = self.imp().workspace.view.borrow_mut().fail_files();
                if outcome.active {
                    self.set_status(error);
                    if !outcome.has_content {
                        self.show_main_surface_error(PlaceholderSurface::Files, error);
                    }
                }
            }
            RuntimeFailureRecovery::SavedItems => {
                let outcome = self.imp().workspace.view.borrow_mut().fail_saved();
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
            RuntimeFailureRecovery::Attachment => self.set_status(error),
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
            RuntimeFailureRecovery::Upload {
                channel_id,
                thread_ts,
            } => {
                let imp = self.imp();
                if let Some(key) = self.draft_key(&channel_id, thread_ts.as_deref()) {
                    imp.pending_upload_drafts.borrow_mut().remove(&key);
                }
                imp.upload_button.set_sensitive(true);
                imp.thread_send_button.set_sensitive(true);
                imp.upload_progress.set_visible(false);
                imp.upload_progress.set_fraction(0.0);
                imp.upload_progress.set_text(Some("Upload failed"));
                if self.mutation_target_is_active(&channel_id, thread_ts.as_deref()) {
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
        let state = self.imp().workspace.view.borrow();
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
        let message = localized_replies_error(error);
        self.thread_pane().show_placeholder(&message);
    }

    fn mark_image_asset_failed(&self, key: &str) {
        let imp = self.imp();
        imp.pending_image_assets.borrow_mut().remove(key);
        imp.failed_image_assets.borrow_mut().insert(key.to_string());
        self.patch_image_asset(key, None);
    }

    fn patch_image_asset(&self, key: &str, source: Option<String>) {
        let (main_view, main_uses_asset, thread_uses_asset) = {
            let state = self.imp().workspace.view.borrow();
            let main = state.visible_channel_id().is_some_and(|channel_id| {
                messages_use_image_asset(state.channel_messages(channel_id), key)
            });
            let thread = state.selected_thread_ts().is_some()
                && messages_use_image_asset(state.current_thread_messages(), key);
            (state.main_view(), main, thread)
        };

        if main_uses_asset {
            self.apply_timeline_patch(
                TimelineSurface::Main,
                message_html::update_image_patch(key, source.clone()),
                UiInvalidations::MAIN,
            );
        } else if !matches!(
            main_view,
            MainMessageView::Conversation | MainMessageView::Placeholder
        ) {
            self.queue_ui_invalidations(UiInvalidations::MAIN);
        }
        if thread_uses_asset {
            self.apply_timeline_patch(
                TimelineSurface::Thread,
                message_html::update_image_patch(key, source),
                UiInvalidations::THREAD,
            );
        }
    }

    fn show_conversation_load_error(&self, error: &str) {
        self.set_sidebar_error(error);
    }

    fn set_sidebar_error(&self, error: &str) {
        let imp = self.imp();
        let has_conversations = !imp.workspace.conversations.borrow().is_empty();
        imp.sidebar_loading.set(false);
        *imp.sidebar_error.borrow_mut() = Some(error.to_string());
        if sidebar_error_change_needs_render(has_conversations) {
            self.render_conversations();
        }
    }

    fn populate_conversations(&self, conversations: Vec<SlackConversation>) {
        let incoming_ids = conversations
            .iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<HashSet<_>>();
        let pending_ids = self.imp().pending_opened_conversation_ids.borrow().clone();
        let preserve_opened = {
            let catalog = self.imp().workspace.conversations.borrow();
            pending_ids
                .iter()
                .filter(|id| !incoming_ids.contains(id.as_str()))
                .filter_map(|id| catalog.get(id).cloned())
                .collect::<Vec<_>>()
        };
        {
            let mut catalog = self.imp().workspace.conversations.borrow_mut();
            let mut snapshot = catalog.begin_membership_snapshot();
            for conversation in conversations {
                snapshot.upsert(conversation);
            }
            if !catalog.commit_membership_snapshot(snapshot) {
                return;
            }
            for conversation in preserve_opened {
                catalog.upsert_opened(conversation);
            }
        }
        self.imp()
            .pending_opened_conversation_ids
            .borrow_mut()
            .clear();
        self.sync_conversations_from_catalog();
    }

    fn sync_conversations_from_catalog(&self) {
        self.imp().sidebar_loading.set(false);
        *self.imp().sidebar_error.borrow_mut() = None;
        self.request_conversation_user_names();
        self.render_conversations();
        if self.current_main_view() == MainMessageView::Unreads {
            self.populate_unreads(self.unread_items());
        } else {
            self.refresh_current_conversation_title();
        }
        self.imp().workspace_ready.set(true);
        self.activate_pending_notification_target();
        self.refresh_open_conversation_picker();
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
            let conversations = imp.workspace.conversations.borrow().conversations();
            changed_user_ids.iter().any(|user_id| {
                sidebar_user_name_update_needs_render(
                    &conversations,
                    user_id,
                    imp.sidebar_loading.get(),
                )
            })
        };
        if should_render_sidebar {
            self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
        }
        for user_id in &changed_user_ids {
            self.patch_user_on_timelines(user_id);
        }
        self.queue_ui_invalidations(UiInvalidations::PICKER | UiInvalidations::TITLE);
    }

    fn populate_user_full_names(&self, names: HashMap<String, String>) {
        if names.is_empty() {
            return;
        }
        let changed = {
            let mut known = self.imp().user_full_names.borrow_mut();
            let mut changed = false;
            for (user_id, full_name) in names {
                changed |= known.get(&user_id) != Some(&full_name);
                known.insert(user_id, full_name);
            }
            changed
        };
        if changed {
            self.queue_ui_invalidations(UiInvalidations::SIDEBAR | UiInvalidations::PICKER);
        }
    }

    fn populate_user_statuses(&self, statuses: HashMap<String, SlackUserStatus>) {
        if statuses.is_empty() {
            return;
        }
        let changed = {
            let mut known = self.imp().user_statuses.borrow_mut();
            statuses
                .into_iter()
                .filter_map(|(user_id, status)| {
                    (known.insert(user_id.clone(), status.clone()).as_ref() != Some(&status))
                        .then_some(user_id)
                })
                .collect::<Vec<_>>()
        };
        self.user_statuses_changed(changed);
    }

    fn replace_user_statuses(&self, statuses: HashMap<String, SlackUserStatus>) {
        let changed = {
            let previous = self.imp().user_statuses.borrow();
            previous
                .keys()
                .chain(statuses.keys())
                .filter(|user_id| previous.get(*user_id) != statuses.get(*user_id))
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        *self.imp().user_statuses.borrow_mut() = statuses;
        self.user_statuses_changed(changed);
    }

    fn user_statuses_changed(&self, changed_user_ids: Vec<String>) {
        for user_id in &changed_user_ids {
            self.patch_user_on_timelines(user_id);
        }
        self.queue_ui_invalidations(
            UiInvalidations::SIDEBAR | UiInvalidations::PICKER | UiInvalidations::TITLE,
        );

        let imp = self.imp();
        let generation = imp.status_expiry_generation.get().saturating_add(1);
        imp.status_expiry_generation.set(generation);
        let now = current_unix_seconds();
        let Some(expiration) = nearest_status_expiration(&imp.user_statuses.borrow(), now) else {
            return;
        };
        let delay = Duration::from_secs(expiration.saturating_sub(now).max(1) as u64);
        let weak_window = self.downgrade();
        glib::timeout_add_local_once(delay, move || {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            if window.imp().status_expiry_generation.get() == generation {
                let user_ids = window
                    .imp()
                    .user_statuses
                    .borrow()
                    .keys()
                    .cloned()
                    .collect();
                window.user_statuses_changed(user_ids);
            }
        });
    }

    fn patch_user_on_timelines(&self, user_id: &str) {
        let (main_view, main_uses_user, main_reaction_user, thread_uses_user, thread_reaction_user) = {
            let state = self.imp().workspace.view.borrow();
            let main_messages = state
                .visible_channel_id()
                .map(|channel_id| state.channel_messages(channel_id));
            let thread_messages = state
                .selected_thread_ts()
                .map(|_| state.current_thread_messages());
            (
                state.main_view(),
                main_messages.is_some_and(|messages| messages_use_user(messages, user_id)),
                main_messages
                    .is_some_and(|messages| messages_use_user_in_reactions(messages, user_id)),
                thread_messages.is_some_and(|messages| messages_use_user(messages, user_id)),
                thread_messages
                    .is_some_and(|messages| messages_use_user_in_reactions(messages, user_id)),
            )
        };
        let name = self
            .imp()
            .user_names
            .borrow()
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string());
        let status = self.imp().user_statuses.borrow().get(user_id).cloned();
        let custom_emojis = self.imp().custom_emojis.borrow().clone();

        if main_reaction_user {
            // Reaction tooltips contain resolved participant names but do not yet
            // expose individual participant nodes for a targeted DOM update.
            self.queue_ui_invalidations(UiInvalidations::MAIN);
        } else if main_uses_user {
            self.apply_timeline_patch(
                TimelineSurface::Main,
                message_html::update_user_patch(user_id, &name, status.as_ref(), &custom_emojis),
                UiInvalidations::MAIN,
            );
        } else if !matches!(
            main_view,
            MainMessageView::Conversation | MainMessageView::Placeholder
        ) {
            self.queue_ui_invalidations(UiInvalidations::MAIN);
        }
        if thread_reaction_user {
            self.queue_ui_invalidations(UiInvalidations::THREAD);
        } else if thread_uses_user {
            self.apply_timeline_patch(
                TimelineSurface::Thread,
                message_html::update_user_patch(user_id, &name, status.as_ref(), &custom_emojis),
                UiInvalidations::THREAD,
            );
        }
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
            self.queue_ui_invalidations(UiInvalidations::MAIN | UiInvalidations::THREAD);
        }
    }

    fn advance_conversation_read_cursor(&self, channel_id: &str, ts: &str) {
        let current_user_id = self.imp().current_user_id.borrow().clone();
        let remaining_unread = self
            .imp()
            .workspace
            .view
            .borrow()
            .channel_messages(channel_id)
            .iter()
            .filter(|message| message.ts.as_str() > ts)
            .filter(|message| message.user.as_deref() != current_user_id.as_deref())
            .count() as u64;
        self.imp()
            .workspace
            .conversations
            .borrow_mut()
            .advance_read_cursor(channel_id, ts, remaining_unread);
    }

    fn apply_conversation_unread_state(&self, channel_id: &str, unread_state: SlackUnreadState) {
        if !unread_state.known {
            return;
        }
        let previous = self
            .imp()
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    conversation.unread_activity_count(),
                )
            });
        self.imp()
            .workspace
            .conversations
            .borrow_mut()
            .apply_realtime_unread(channel_id, unread_state);
        let current = self
            .imp()
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    conversation.unread_activity_count(),
                )
            });
        let changed = previous != current;

        if changed {
            self.render_conversations();
            if self.current_main_view() == MainMessageView::Unreads {
                self.populate_unreads(self.unread_items());
            }
        }
    }

    fn mark_conversation_locally_unread(&self, channel_id: &str) -> bool {
        let existing_unread_count = self
            .imp()
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(SlackConversation::unread_activity_count);
        let unread_count = existing_unread_count.unwrap_or_default().saturating_add(1);
        self.imp()
            .workspace
            .conversations
            .borrow_mut()
            .apply_realtime_unread(
                channel_id,
                SlackUnreadState::from_parts(true, true, unread_count),
            );
        existing_unread_count.is_some()
    }

    fn channel_load_more_url(&self, channel_id: &str) -> Option<String> {
        self.imp()
            .workspace
            .view
            .borrow()
            .channel_cursor(channel_id)
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, None))
    }

    fn thread_load_more_url(&self, channel_id: &str, ts: &str) -> Option<String> {
        self.imp()
            .workspace
            .view
            .borrow()
            .thread_cursor()
            .map(|cursor| message_html::load_more_action_url(channel_id, cursor, Some(ts)))
    }

    fn render_conversations(&self) {
        let started = Instant::now();
        self.sync_workspace_chrome();
        let imp = self.imp();
        let conversations = imp.workspace.conversations.borrow().conversations();
        let user_names = imp.user_names.borrow().clone();
        let user_search_aliases = imp.user_search_aliases.borrow();
        let selected_channel = self.visible_channel_id();
        let model = sidebar::build_sidebar_list(
            &conversations,
            &user_names,
            sidebar::SidebarBuildOptions {
                selected_channel: selected_channel.as_deref(),
                current_user_id: imp.current_user_id.borrow().as_deref(),
                query: imp.sidebar_filter_entry.text().as_str(),
                unread_only: imp.sidebar_unread_filter_button.is_active(),
                show_unreads_section: self.show_unreads_section(),
                loading: imp.sidebar_loading.get(),
                has_error: imp.sidebar_error.borrow().is_some(),
                user_search_aliases: Some(&user_search_aliases),
                user_full_names: Some(&imp.user_full_names.borrow()),
                user_statuses: Some(&imp.user_statuses.borrow()),
            },
        );

        self.reconcile_sidebar(model.keyed_items());
        log_performance(started, |elapsed_ms| {
            format!(
                "sidebar_render conversations={} elapsed_ms={:.2}",
                conversations.len(),
                elapsed_ms
            )
        });
    }

    fn show_unreads_section(&self) -> bool {
        self.imp()
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.boolean(config::SIDEBAR_SHOW_UNREADS_SECTION_KEY))
            .unwrap_or(false)
    }

    fn sidebar_item_row(&self, item: &KeyedSidebarItem) -> gtk::ListBoxRow {
        match &item.model {
            SidebarItemModel::Placeholder(placeholder) => {
                let row = gtk::ListBoxRow::new();
                row.set_selectable(false);
                row.set_activatable(false);
                row.set_child(Some(&self.placeholder_label(placeholder.label())));
                row
            }
            SidebarItemModel::SectionHeader { title, .. } => self.sidebar_section_row(title),
            SidebarItemModel::Conversation(model) => {
                let row = sidebar_row_widget(
                    model,
                    SidebarRowLayout::sidebar(),
                    &self.imp().custom_emojis.borrow(),
                );
                self.attach_sidebar_context_menu(&row, &model.id);
                row
            }
        }
    }

    fn attach_sidebar_context_menu(&self, row: &gtk::ListBoxRow, channel_id: &str) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let weak_window = self.downgrade();
        let row_for_menu = row.clone();
        let channel_id = channel_id.to_string();
        gesture.connect_pressed(move |_, _, x, y| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let popover = gtk::Popover::new();
            popover.set_parent(&row_for_menu);
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
            menu.set_margin_top(6);
            menu.set_margin_bottom(6);
            menu.set_margin_start(6);
            menu.set_margin_end(6);

            let mark_read_button = gtk::Button::with_label(&gettext("Mark as read"));
            mark_read_button.add_css_class("flat");
            let mark_read_channel_id = channel_id.clone();
            let weak_window = window.downgrade();
            mark_read_button.connect_clicked(move |_| {
                if let Some(window) = weak_window.upgrade() {
                    window.mark_channel_read_through_latest(&mark_read_channel_id);
                }
            });
            menu.append(&mark_read_button);

            let conversation = window
                .imp()
                .workspace
                .conversations
                .borrow()
                .get(&channel_id)
                .cloned();
            if let Some(conversation) = conversation.filter(sidebar_conversation_can_leave) {
                let leave_button = gtk::Button::with_label(&gettext("Leave channel"));
                leave_button.add_css_class("flat");
                leave_button.add_css_class("destructive-action");
                let weak_window = window.downgrade();
                let popover_for_leave = popover.clone();
                leave_button.connect_clicked(move |_| {
                    popover_for_leave.popdown();
                    let Some(window) = weak_window.upgrade() else {
                        return;
                    };
                    if sidebar_conversation_leave_requires_confirmation(&conversation) {
                        window.confirm_leave_private_channel(&conversation);
                    } else {
                        window.leave_channel(&conversation.id);
                    }
                });
                menu.append(&leave_button);
            }

            popover.set_child(Some(&menu));
            popover.popup();
        });
        row.add_controller(gesture);
    }

    fn confirm_leave_private_channel(&self, conversation: &SlackConversation) {
        let channel_name = conversation.display_name();
        let dialog = adw::AlertDialog::builder()
            .heading(format!("{} {channel_name}?", gettext("Leave")))
            .body(gettext(
                "You won't be able to rejoin this private channel unless someone invites you again.",
            ))
            .default_response("cancel")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("leave", &gettext("Leave channel"));
        dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        let channel_id = conversation.id.clone();
        let weak_window = self.downgrade();
        dialog.connect_response(Some("leave"), move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.leave_channel(&channel_id);
            }
        });
        dialog.present(Some(self));
    }

    fn leave_channel(&self, channel_id: &str) {
        self.send_command(RuntimeCommand::LeaveConversation {
            channel_id: channel_id.to_string(),
        });
    }

    fn apply_conversation_left(&self, channel_id: &str) {
        let was_visible = self.visible_channel_id().as_deref() == Some(channel_id);
        let removed = self
            .imp()
            .workspace
            .conversations
            .borrow_mut()
            .remove(channel_id);
        self.imp()
            .workspace
            .view
            .borrow_mut()
            .remove_conversation(channel_id);

        if removed
            .as_ref()
            .is_some_and(sidebar_conversation_leave_requires_confirmation)
        {
            self.imp()
                .discovered_channels
                .borrow_mut()
                .retain(|conversation| conversation.id != channel_id);
        }
        self.imp()
            .pending_opened_conversation_ids
            .borrow_mut()
            .remove(channel_id);
        self.imp()
            .latest_message_ts_by_channel
            .borrow_mut()
            .remove(channel_id);
        self.imp()
            .local_read_ts_by_channel
            .borrow_mut()
            .remove(channel_id);

        if was_visible {
            let title = gettext("Select a conversation");
            self.imp().message_title.set_title(&title);
            self.show_message_placeholder(&title);
            self.render_closed_thread();
        }
        self.sync_conversations_from_catalog();
        self.refresh_open_conversation_picker();
        self.set_status(&gettext("Left channel"));
    }

    fn mark_channel_read_through_latest(&self, channel_id: &str) {
        let latest = self
            .imp()
            .latest_message_ts_by_channel
            .borrow()
            .get(channel_id)
            .cloned()
            .or_else(|| {
                self.imp()
                    .workspace
                    .conversations
                    .borrow()
                    .get(channel_id)
                    .and_then(SlackConversation::latest_message_ts)
                    .map(ToString::to_string)
            });
        if let Some(ts) = latest {
            self.send_command(RuntimeCommand::MarkConversationRead {
                channel_id: channel_id.to_string(),
                ts,
            });
        } else {
            self.set_status(&gettext("No message available to mark as read"));
        }
    }

    fn sidebar_section_row(&self, title: &str) -> gtk::ListBoxRow {
        let header_row = gtk::ListBoxRow::new();
        header_row.set_selectable(false);
        header_row.set_activatable(false);
        header_row.set_focusable(false);

        let header = gtk::Label::new(Some(title));
        header.set_xalign(0.0);
        header.set_margin_top(12);
        header.set_margin_bottom(3);
        header.set_margin_start(9);
        header.set_margin_end(9);
        header.add_css_class("caption");
        header.add_css_class("heading");

        header_row.set_child(Some(&header));
        header_row
    }

    fn reconcile_sidebar(&self, next_items: Vec<KeyedSidebarItem>) {
        let imp = self.imp();
        let previous_items = imp.sidebar_items.borrow().clone();
        let diff = diff_keyed_sidebar_items(&previous_items, &next_items);
        let previous_keys = previous_items
            .iter()
            .map(|item| &item.key)
            .collect::<Vec<_>>();
        let next_keys = next_items.iter().map(|item| &item.key).collect::<Vec<_>>();
        let structure_changed = previous_keys != next_keys;

        {
            let mut rows = imp.sidebar_rows.borrow_mut();
            for key in &diff.removed {
                rows.remove(key);
            }
            for (_, key) in &diff.inserted {
                if let Some(item) = next_items.iter().find(|item| &item.key == key) {
                    rows.insert(key.clone(), self.sidebar_item_row(item));
                }
            }
            for (key, index) in &diff.updated {
                let Some(existing) = rows.get(key) else {
                    continue;
                };
                let replacement = self.sidebar_item_row(&next_items[*index]);
                existing.set_selectable(replacement.is_selectable());
                existing.set_activatable(replacement.is_activatable());
                existing.set_focusable(replacement.is_focusable());
                existing.set_tooltip_text(replacement.tooltip_text().as_deref());
                if let SidebarItemModel::Conversation(model) = &next_items[*index].model {
                    existing.update_property(&[gtk::accessible::Property::Label(
                        &model.accessible_label(),
                    )]);
                }
                if let Some(child) = replacement.child() {
                    replacement.set_child(None::<&gtk::Widget>);
                    existing.set_child(Some(&child));
                }
            }
        }

        if structure_changed {
            self.clear_list(&imp.conversation_list);
            let rows = imp.sidebar_rows.borrow();
            for item in &next_items {
                if let Some(row) = rows.get(&item.key) {
                    imp.conversation_list.append(row);
                }
            }
        }

        imp.sidebar_row_actions.borrow_mut().clear();
        let rows = imp.sidebar_rows.borrow();
        let mut selected = None;
        for item in &next_items {
            let Some(row) = rows.get(&item.key) else {
                continue;
            };
            if let SidebarItemModel::Conversation(model) = &item.model {
                self.register_sidebar_row_action(row.index(), model);
                if model.selected && selected.is_none() {
                    selected = Some(row);
                }
            }
        }
        imp.conversation_list.select_row(selected);
        drop(rows);
        *imp.sidebar_items.borrow_mut() = next_items;
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
            let title = self.conversation_title(&action.channel_id);
            self.select_conversation(&action.channel_id, &title);
        }
    }

    fn show_conversation_switcher(&self) {
        self.send_command(RuntimeCommand::DiscoverChannels);
        self.show_conversation_picker(
            "Switch conversation",
            "Search conversations",
            true,
            |window, action| match action.action {
                ConversationPickerAction::OpenConversation => {
                    let title = window.conversation_title(&action.channel_id);
                    window.select_conversation(&action.channel_id, &title)
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
        let conversations = imp.workspace.conversations.borrow().conversations();
        let user_names = imp.user_names.borrow().clone();
        let discovered_channels = imp.discovered_channels.borrow().clone();
        let discovered_users = imp.discovered_users.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        let user_search_aliases = imp.user_search_aliases.borrow().clone();
        let user_statuses = imp.user_statuses.borrow().clone();
        let sections = picker_sections(
            include_discovery,
            sidebar::ConversationPickerSource {
                conversations: &conversations,
                discovered_channels: &discovered_channels,
                discovered_users: &discovered_users,
                user_names: &user_names,
                current_user_id: current_user_id.as_deref(),
                known_user_search_aliases: &user_search_aliases,
                user_full_names: &imp.user_full_names.borrow(),
                user_statuses: &user_statuses,
            },
            "",
        );
        if picker_sections_empty(&sections) && !include_discovery {
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

        *self.imp().conversation_picker_view.borrow_mut() = Some(ConversationPickerView {
            list: list.clone(),
            search: search.clone(),
            actions: actions.clone(),
            include_discovery,
        });

        let weak_window = self.downgrade();
        search.connect_search_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.schedule_picker_filter();
            }
        });

        let weak_window = self.downgrade();
        let list_for_close = list.clone();
        dialog.connect_close_request(move |_| {
            if let Some(window) = weak_window.upgrade() {
                let mut active = window.imp().conversation_picker_view.borrow_mut();
                if active
                    .as_ref()
                    .is_some_and(|view| view.list == list_for_close)
                {
                    active.take();
                }
            }
            glib::Propagation::Proceed
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

    fn refresh_open_conversation_picker(&self) {
        let Some(view) = self.imp().conversation_picker_view.borrow().clone() else {
            return;
        };
        let query = view.search.text();
        let sections = {
            let imp = self.imp();
            let conversations = imp.workspace.conversations.borrow().conversations();
            picker_sections(
                view.include_discovery,
                sidebar::ConversationPickerSource {
                    conversations: &conversations,
                    discovered_channels: &imp.discovered_channels.borrow(),
                    discovered_users: &imp.discovered_users.borrow(),
                    user_names: &imp.user_names.borrow(),
                    current_user_id: imp.current_user_id.borrow().as_deref(),
                    known_user_search_aliases: &imp.user_search_aliases.borrow(),
                    user_full_names: &imp.user_full_names.borrow(),
                    user_statuses: &imp.user_statuses.borrow(),
                },
                query.as_str(),
            )
        };
        self.populate_conversation_picker_list(&view.list, &view.actions, &sections);
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

        if let Some(results) = sections.search_results.as_deref() {
            for item in results {
                self.append_conversation_picker_row(list, actions, item);
            }
            return;
        }

        for (title, items) in [
            ("Conversations", sections.conversations.as_slice()),
            ("Channels you can join", sections.channels.as_slice()),
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
        let row = sidebar_row_widget(
            &item.row,
            SidebarRowLayout::switcher(),
            &self.imp().custom_emojis.borrow(),
        );
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
                self.refresh_conversation_title_status(&channel_id);
            }
        }
    }

    fn refresh_conversation_title_status(&self, channel_id: &str) {
        let imp = self.imp();
        let status = imp
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
            .filter(|conversation| conversation.is_im.unwrap_or(false))
            .and_then(|conversation| conversation.user.as_deref().map(str::to_string))
            .and_then(|user_id| imp.user_statuses.borrow().get(&user_id).cloned())
            .filter(|status| status.active_at(current_unix_seconds()));
        if let Some(status) = status {
            let emoji = crate::emoji::EmojiCatalog::new(&imp.custom_emojis.borrow())
                .resolve(status.emoji_name())
                .and_then(|value| match value {
                    crate::emoji::EmojiValue::Unicode(glyph) => Some(glyph.to_string()),
                    crate::emoji::EmojiValue::CustomImage(_) => None,
                })
                .unwrap_or_else(|| "●".to_string());
            let text = status.accessible_text();
            imp.message_title.set_subtitle(&format!("{emoji} {text}"));
            imp.message_title.set_tooltip_text(Some(&text));
            imp.message_title
                .update_property(&[gtk::accessible::Property::Description(&format!(
                    "Status: {text}"
                ))]);
        } else {
            imp.message_title.set_subtitle("");
            imp.message_title.set_tooltip_text(None);
            imp.message_title
                .update_property(&[gtk::accessible::Property::Description("")]);
        }
    }

    fn sync_workspace_chrome(&self) {
        let imp = self.imp();
        let main_view = imp.workspace.view.borrow().main_view();
        let selection = workspace_navigation_selection(main_view);
        imp.messages_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Messages));
        imp.unreads_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Unreads));
        imp.threads_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Threads));
        imp.files_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Files));
        imp.saved_button
            .set_active(selection == Some(WorkspaceNavigationSelection::Saved));
        imp.message_composer
            .set_visible(workspace_composer_visible(main_view));
        if main_view != MainMessageView::Conversation {
            imp.message_title.set_subtitle("");
            imp.message_title.set_tooltip_text(None);
            imp.message_title
                .update_property(&[gtk::accessible::Property::Description("")]);
        }
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
            .workspace
            .view
            .borrow_mut()
            .select_conversation(channel_id);
        let current_messages = imp.workspace.view.borrow().snapshot().channel_messages;
        imp.message_title.set_title(title);
        self.refresh_conversation_title_status(channel_id);
        self.restore_channel_draft(channel_id);
        set_text_view_text(&imp.thread_entry, "");
        self.thread_pane().close();
        imp.workspace_split.set_show_content(true);
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
            .workspace
            .view
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
            .workspace
            .view
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
        if !imp.workspace.view.borrow().has_channel_context(channel_id) {
            context.load_more_url = self.channel_load_more_url(channel_id);
        }
        let (has_unread, last_read, unread_count) = imp
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    imp.local_read_ts_by_channel
                        .borrow()
                        .get(channel_id)
                        .cloned()
                        .or_else(|| conversation.last_read_ts().map(ToString::to_string)),
                    conversation.unread_activity_count(),
                )
            })
            .unwrap_or_default();
        let first_unread_ts = has_unread
            .then(|| first_unread_message_ts(&messages, last_read.as_deref(), unread_count))
            .flatten();
        if context.thread_ts.is_none() {
            context.read_marker_url = Some(message_html::mark_read_action_url(channel_id, "0"));
        }
        if first_unread_ts.is_some() {
            context.first_unread_ts = first_unread_ts.clone();
        }
        context.timeline_scroll = scroll_behavior;
        let explicit_focus_ts = imp
            .workspace
            .view
            .borrow_mut()
            .take_channel_focus_for_render(channel_id, &messages);
        let unread_focus_ts = first_unread_ts;
        let focus_message_ts = explicit_focus_ts.or(unread_focus_ts);
        if focus_message_ts.is_some() && has_unread {
            context.timeline_scroll = TimelineScrollBehavior::Preserve;
        }
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
        let html = generate_html("conversation", || {
            message_html::conversation_document_with_focus(
                channel_id,
                &messages,
                &context,
                focus_message_ts.as_deref(),
            )
        });
        self.load_message_html(&html);
        self.queue_history_render_followups(channel_id, messages);
    }

    fn queue_history_render_followups(&self, channel_id: &str, messages: Vec<SlackMessage>) {
        let weak_window = self.downgrade();
        let channel_id = channel_id.to_string();
        glib::idle_add_local_once(move || {
            if let Some(window) = weak_window.upgrade() {
                window.queue_ui_invalidations(UiInvalidations::SIDEBAR);
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
        self.request_image_assets(messages.iter());
        let mut context = self.message_html_context(Some(ts));
        if !imp
            .workspace
            .view
            .borrow()
            .has_thread_context(channel_id, ts)
        {
            context.load_more_url = self.thread_load_more_url(channel_id, ts);
        }
        context.timeline_scroll = scroll_behavior;
        context.read_marker_url = SlackMessage::latest_ts(messages.iter())
            .map(|latest_ts| message_html::mark_thread_read_action_url(channel_id, ts, &latest_ts));
        let focus_message_ts = imp
            .workspace
            .view
            .borrow_mut()
            .take_thread_focus_for_render(channel_id, ts, &messages);
        self.thread_pane()
            .render(channel_id, &messages, &context, focus_message_ts.as_deref());
    }

    fn populate_unreads(&self, items: Vec<ActivityItem>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Unreads"));
        self.render_conversations();
        self.load_message_html(&message_html::unreads_document(&items));
    }

    fn populate_threads(&self) {
        let observed = self.imp().workspace.view.borrow().observed_threads();
        let observed = self
            .imp()
            .workspace
            .threads
            .borrow()
            .inbox_projection(observed);
        let roots = observed
            .iter()
            .map(|(_, message)| message.clone())
            .collect::<Vec<_>>();
        self.request_user_names(&roots);
        self.request_image_assets(roots.iter());
        let items = observed
            .into_iter()
            .map(|(channel_id, root)| message_html::ThreadInboxItem {
                channel_title: self.conversation_title(&channel_id),
                channel_id,
                root,
            })
            .collect::<Vec<_>>();
        self.imp().message_title.set_title(&gettext("Threads"));
        let context = self.message_html_context(None);
        self.load_message_html(&message_html::threads_document(&items, &context));
    }

    fn populate_search_results(&self, results: Vec<SearchMatch>) {
        let imp = self.imp();
        imp.message_title.set_title(&gettext("Search results"));
        self.request_user_ids(
            results
                .iter()
                .filter_map(|result| result.user.clone())
                .collect(),
        );
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
            SocketModeEvent::UserChanged(user) => {
                let Some(user_id) = user.id.clone() else {
                    return;
                };
                if let Some(display_name) = user.display_name() {
                    self.populate_user_names(HashMap::from([(user_id.clone(), display_name)]));
                }
                if let Some(full_name) = user.full_name() {
                    self.populate_user_full_names(HashMap::from([(user_id.clone(), full_name)]));
                }
                let mut statuses = self.imp().user_statuses.borrow().clone();
                match user.status() {
                    Some(status) => {
                        statuses.insert(user_id, status);
                    }
                    None => {
                        statuses.remove(&user_id);
                    }
                }
                self.replace_user_statuses(statuses);
            }
            SocketModeEvent::RefreshConversations => self.refresh_conversations(),
        }
    }

    fn apply_socket_message(&self, event: SocketModeMessageEvent) {
        let channel_id = event.channel_id.clone();
        let message = event.message.clone();
        let reading_channel = self.visible_channel_id();
        let current_user_id = self.imp().current_user_id.borrow().clone();

        let first_delivery = event.kind != SocketModeMessageKind::Posted
            || self
                .imp()
                .seen_realtime_messages
                .borrow_mut()
                .insert(format!("{}:{}", event.channel_id, event.message.ts));
        let should_mark_unread = first_delivery
            && realtime_message_marks_unread(
                reading_channel.as_deref(),
                self.is_active(),
                current_user_id.as_deref(),
                &event,
            );
        let was_unread = self
            .imp()
            .workspace
            .conversations
            .borrow()
            .get(&channel_id)
            .is_some_and(SlackConversation::has_unread_activity);
        if should_mark_unread && !self.mark_conversation_locally_unread(&channel_id) {
            self.refresh_conversations();
        }
        let became_unread = should_mark_unread && !was_unread;

        let kind = match event.kind {
            SocketModeMessageKind::Posted => RealtimeMessageKind::Posted,
            SocketModeMessageKind::Changed => RealtimeMessageKind::Changed,
            SocketModeMessageKind::Deleted => RealtimeMessageKind::Deleted,
        };
        let (channel_dom_kind, thread_dom_kind) = {
            let state = self.imp().workspace.view.borrow();
            let channel_kind =
                realtime_dom_patch_kind(kind, state.channel_messages(&channel_id), &message);
            let thread_kind = state
                .selected_thread_ts()
                .filter(|thread_ts| {
                    message.thread_ts.as_deref() == Some(*thread_ts) && message.ts != **thread_ts
                })
                .map(|_| realtime_dom_patch_kind(kind, state.current_thread_messages(), &message))
                .unwrap_or(Some(kind));
            (channel_kind, thread_kind)
        };

        let outcome = self
            .imp()
            .workspace
            .view
            .borrow_mut()
            .apply_realtime_message(&channel_id, message.clone(), kind);

        if outcome.render_channel {
            if self
                .imp()
                .workspace
                .view
                .borrow()
                .has_channel_context(&channel_id)
            {
                self.queue_ui_invalidations(UiInvalidations::MAIN);
            } else if let Some(dom_kind) = channel_dom_kind {
                self.apply_realtime_message_patch(
                    TimelineSurface::Main,
                    &channel_id,
                    &message,
                    dom_kind,
                    became_unread,
                    None,
                    UiInvalidations::MAIN,
                );
            } else {
                self.queue_ui_invalidations(UiInvalidations::MAIN);
            }
        }

        if outcome.render_thread {
            if let Some(thread_ts) = self.selected_thread_ts() {
                if self
                    .imp()
                    .workspace
                    .view
                    .borrow()
                    .has_thread_context(&channel_id, &thread_ts)
                {
                    self.queue_ui_invalidations(UiInvalidations::THREAD);
                } else if let Some(dom_kind) = thread_dom_kind {
                    self.apply_realtime_message_patch(
                        TimelineSurface::Thread,
                        &channel_id,
                        &message,
                        dom_kind,
                        false,
                        Some(&thread_ts),
                        UiInvalidations::THREAD,
                    );
                } else {
                    self.queue_ui_invalidations(UiInvalidations::THREAD);
                }
            }
        }

        if event.kind == SocketModeMessageKind::Posted {
            self.notify_if_new_messages(
                &channel_id,
                std::slice::from_ref(&message),
                MessageNotificationDelivery::Realtime { first_delivery },
            );
        }
        self.request_user_names(std::slice::from_ref(&message));
        self.request_image_assets(std::iter::once(&message));

        if outcome.refresh_unreads {
            self.populate_unreads(self.unread_items());
        } else {
            self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
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
            .workspace
            .view
            .borrow_mut()
            .apply_reaction(&update);

        if outcome.changed {
            let updated_message = self
                .imp()
                .workspace
                .view
                .borrow()
                .find_message(&update.channel_id, &update.ts);
            let Some(updated_message) = updated_message else {
                self.queue_ui_invalidations(UiInvalidations::MAIN | UiInvalidations::THREAD);
                return;
            };
            if outcome.render_channel {
                let patch = message_html::message_region_patch(
                    &update.channel_id,
                    &updated_message,
                    &self.message_patch_context(None, &updated_message),
                    TimelineMessageRegion::Responses,
                );
                self.apply_timeline_patch(TimelineSurface::Main, patch, UiInvalidations::MAIN);
            }
            if outcome.render_thread {
                let thread_ts = self.selected_thread_ts();
                let patch = message_html::message_region_patch(
                    &update.channel_id,
                    &updated_message,
                    &self.message_patch_context(thread_ts.as_deref(), &updated_message),
                    TimelineMessageRegion::Responses,
                );
                self.apply_timeline_patch(TimelineSurface::Thread, patch, UiInvalidations::THREAD);
            }
        }
    }

    fn notify_if_new_messages(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
        delivery: MessageNotificationDelivery,
    ) {
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

        self.imp().latest_message_ts_by_channel.borrow_mut().insert(
            channel_id.to_string(),
            notification_baseline_after(previous_ts.as_deref(), &latest_ts),
        );

        let actively_reading = actively_reading_channel(
            self.is_active(),
            self.visible_channel_id().as_deref(),
            channel_id,
        );
        let (has_unread, muted) = self.notification_conversation_state(channel_id);
        let action = message_notification_action(MessageNotificationState {
            previous_latest_ts: previous_ts.as_deref(),
            latest_ts: latest_ts.as_str(),
            latest_message_user: latest_message.and_then(|message| message.user.as_deref()),
            current_user: current_user_id.as_deref(),
            has_unread,
            muted,
            actively_reading,
            delivery,
        });

        if action == MessageNotificationAction::Notify {
            self.send_notification(
                channel_id,
                &self.navigation_conversation_title(channel_id),
                &message_notification_body(latest_message),
            );
        }
    }

    fn notification_conversation_state(&self, channel_id: &str) -> (bool, bool) {
        self.imp()
            .workspace
            .conversations
            .borrow()
            .get(channel_id)
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

    pub(crate) fn open_notification_target(
        &self,
        workspace_id: String,
        channel_id: String,
    ) -> bool {
        let expected_channel_id = channel_id.clone();
        *self.imp().pending_notification_target.borrow_mut() = Some(NotificationTarget {
            workspace_id,
            channel_id,
        });
        self.activate_pending_notification_target();
        self.imp().pending_notification_target.borrow().is_none()
            && self.visible_channel_id().as_deref() == Some(expected_channel_id.as_str())
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
        let imp = self.imp();
        let user_names = imp.user_names.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        imp.workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(|conversation| {
                conversation.display_name_with_users(&user_names, current_user_id.as_deref())
            })
            .unwrap_or_else(|| "Slack".to_string())
    }

    fn navigation_conversation_title(&self, channel_id: &str) -> String {
        let imp = self.imp();
        let user_names = imp.user_names.borrow();
        let user_full_names = imp.user_full_names.borrow();
        let current_user_id = imp.current_user_id.borrow();
        imp.workspace
            .conversations
            .borrow()
            .get(channel_id)
            .map(|conversation| {
                conversation.navigation_name_with_users(
                    &user_names,
                    &user_full_names,
                    current_user_id.as_deref(),
                )
            })
            .unwrap_or_else(|| "Slack".to_string())
    }

    fn unread_items(&self) -> Vec<ActivityItem> {
        let imp = self.imp();
        let conversations = imp.workspace.conversations.borrow().conversations();
        let user_names = imp.user_names.borrow();
        let current_user_id = imp.current_user_id.borrow();
        let mut items =
            activity::build_activity_items(&conversations, &user_names, current_user_id.as_deref());
        let conversation_titles = conversations
            .iter()
            .map(|conversation| {
                (
                    conversation.id.clone(),
                    conversation.display_name_with_users(&user_names, current_user_id.as_deref()),
                )
            })
            .collect::<HashMap<_, _>>();
        items.extend(activity::build_thread_activity_items(
            imp.workspace.threads.borrow().clone().into_records(),
            &conversation_titles,
        ));
        activity::sort_activity_items(&mut items);
        items
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
        row.set_child(Some(&self.placeholder_label(text)));
        list.append(&row);
    }

    fn placeholder_label(&self, text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        label
    }

    fn show_message_placeholder(&self, text: &str) {
        self.load_message_html(&message_html::placeholder_document(
            &gettext("Messages"),
            text,
        ));
    }

    fn load_message_html(&self, html: &str) {
        if let Some(web_view) = self.imp().message_view.borrow().as_ref() {
            let started = Instant::now();
            crate::debug::log("ui", &format!("load_message_html bytes={}", html.len()));
            web_view.load_html(html, Some(message_html::base_uri()));
            log_performance(started, |elapsed_ms| {
                format!(
                    "html_load_submit surface=main bytes={} elapsed_ms={:.2}",
                    html.len(),
                    elapsed_ms
                )
            });
        }
    }

    fn thread_pane(&self) -> ThreadPane {
        self.imp()
            .thread_pane_controller
            .borrow()
            .as_ref()
            .expect("thread pane should be initialized")
            .clone()
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
            .workspace
            .conversations
            .borrow()
            .conversations()
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

    fn rerender_current_main_messages(&self) {
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
            MainMessageView::Threads => self.populate_threads(),
            MainMessageView::Search => self.populate_search_results(snapshot.search_results),
            MainMessageView::Files => self.populate_files(snapshot.files),
            MainMessageView::Saved => self.populate_saved_items(snapshot.saved_items),
            MainMessageView::Placeholder => {}
        }
    }

    fn rerender_current_thread(&self) {
        let snapshot = self.current_message_snapshot();
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
        self.message_html_context_with_image_keys(thread_ts, None)
    }

    fn message_patch_context(
        &self,
        thread_ts: Option<&str>,
        message: &SlackMessage,
    ) -> MessageHtmlContext {
        let image_keys = message
            .files
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(image_asset_request)
            .map(|(key, _)| key)
            .collect::<HashSet<_>>();
        self.message_html_context_with_image_keys(thread_ts, Some(&image_keys))
    }

    fn message_html_context_with_image_keys(
        &self,
        thread_ts: Option<&str>,
        image_keys: Option<&HashSet<String>>,
    ) -> MessageHtmlContext {
        let imp = self.imp();
        let user_names = imp.user_names.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        let conversation_titles = imp
            .workspace
            .conversations
            .borrow()
            .conversations()
            .into_iter()
            .map(|conversation| {
                let title =
                    conversation.display_name_with_users(&user_names, current_user_id.as_deref());
                (conversation.id, title)
            })
            .collect();
        let recent_reactions = imp
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.strv(config::RECENT_REACTIONS_KEY))
            .map(|names| names.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        MessageHtmlContext {
            user_names,
            conversation_titles,
            user_statuses: imp.user_statuses.borrow().clone(),
            user_group_names: imp.user_group_names.borrow().clone(),
            user_group_members: imp.user_group_members.borrow().clone(),
            current_user_id,
            thread_ts: thread_ts.map(ToString::to_string),
            load_more_url: None,
            timeline_scroll: TimelineScrollBehavior::Preserve,
            image_assets: imp
                .image_assets
                .borrow()
                .iter()
                .filter(|(key, _)| image_keys.is_none_or(|keys| keys.contains(*key)))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            failed_image_urls: imp
                .failed_image_assets
                .borrow()
                .iter()
                .filter(|key| image_keys.is_none_or(|keys| keys.contains(*key)))
                .cloned()
                .collect(),
            recent_reactions,
            custom_emojis: imp.custom_emojis.borrow().clone(),
            read_marker_url: None,
            first_unread_ts: None,
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
        self.imp().workspace.view.borrow().snapshot()
    }

    fn selected_channel_id(&self) -> Option<String> {
        self.imp()
            .workspace
            .view
            .borrow()
            .last_channel_id()
            .map(ToString::to_string)
    }

    fn visible_channel_id(&self) -> Option<String> {
        self.imp()
            .workspace
            .view
            .borrow()
            .visible_channel_id()
            .map(ToString::to_string)
    }

    fn selected_thread_ts(&self) -> Option<String> {
        self.imp()
            .workspace
            .view
            .borrow()
            .selected_thread_ts()
            .map(ToString::to_string)
    }

    fn current_main_view(&self) -> MainMessageView {
        self.imp().workspace.view.borrow().main_view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_presentation_owns_connection_surface_and_status() {
        let cases = [
            (
                WorkspaceLifecycle::Disconnected,
                WorkspaceLifecycleSurface::Connect,
                "Choose a workspace to continue",
            ),
            (
                WorkspaceLifecycle::Connecting,
                WorkspaceLifecycleSurface::Loading,
                "Connecting to Slack…",
            ),
            (
                WorkspaceLifecycle::Syncing,
                WorkspaceLifecycleSurface::Workspace,
                "Syncing workspace…",
            ),
            (
                WorkspaceLifecycle::Ready,
                WorkspaceLifecycleSurface::Workspace,
                "",
            ),
            (
                WorkspaceLifecycle::Degraded,
                WorkspaceLifecycleSurface::Workspace,
                "Connection interrupted. Retrying…",
            ),
            (
                WorkspaceLifecycle::AuthenticationRequired,
                WorkspaceLifecycleSurface::Connect,
                "Slack authentication failed. Sign in again.",
            ),
            (
                WorkspaceLifecycle::StartupFailed,
                WorkspaceLifecycleSurface::Connect,
                "Conduit could not start.",
            ),
        ];

        for (lifecycle, surface, status) in cases {
            assert_eq!(
                workspace_lifecycle_presentation(lifecycle, true),
                WorkspaceLifecyclePresentation { surface, status }
            );
        }

        assert_eq!(
            workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, false).surface,
            WorkspaceLifecycleSurface::Connect
        );
    }

    #[test]
    fn repeated_ui_invalidations_require_only_one_scheduled_flush() {
        let mut pending = UiInvalidations::default();
        let schedules = (0..100)
            .filter(|_| pending.insert(UiInvalidations::MAIN | UiInvalidations::THREAD))
            .count();

        assert_eq!(schedules, 1);
        assert!(pending.contains(UiInvalidations::MAIN));
        assert!(pending.contains(UiInvalidations::THREAD));
    }

    #[test]
    fn coalesced_ui_invalidation_flush_drains_each_surface_once() {
        let mut pending = UiInvalidations::default();
        assert!(pending.insert(UiInvalidations::SIDEBAR));
        assert!(!pending.insert(UiInvalidations::MAIN | UiInvalidations::PICKER));
        assert!(!pending.insert(UiInvalidations::SIDEBAR | UiInvalidations::TITLE));

        let drained = pending.take();
        for surface in [
            UiInvalidations::SIDEBAR,
            UiInvalidations::MAIN,
            UiInvalidations::TITLE,
            UiInvalidations::PICKER,
        ] {
            assert!(drained.contains(surface));
        }
        assert!(!drained.contains(UiInvalidations::THREAD));
        assert_eq!(pending, UiInvalidations::default());
        assert!(pending.insert(UiInvalidations::THREAD));
    }

    #[test]
    fn media_zoom_scales_below_fit_size_without_distorting_aspect_ratio() {
        assert_eq!(media_zoom_size((1600, 900), (800, 600), 1.0), (800, 450));
        assert_eq!(media_zoom_size((1600, 900), (800, 600), 0.5), (400, 225));
        assert_eq!(media_zoom_size((400, 200), (800, 600), 0.25), (100, 50));
    }
    use crate::runtime::{RuntimeOperation, RuntimeTarget};
    use crate::sidebar::ConversationKind;

    #[test]
    fn connected_workspace_slack_permalink_resolves_to_internal_message() {
        let location = slack_message_location(
            "https://signicat.slack.com/archives/C032HRKUBHQ/p1783592777735299",
            Some("https://signicat.slack.com/"),
        )
        .expect("permalink should resolve");

        assert_eq!(location.channel_id(), "C032HRKUBHQ");
        assert_eq!(location.message_ts(), "1783592777.735299");
        assert_eq!(location.thread_ts(), None);
    }

    #[test]
    fn slack_reply_permalink_preserves_thread_root() {
        let location = slack_message_location(
            "https://signicat.slack.com/archives/C123/p1783592777735299?thread_ts=1783500000.000001&cid=C123",
            Some("https://signicat.slack.com"),
        )
        .expect("reply permalink should resolve");

        assert_eq!(location.message_ts(), "1783592777.735299");
        assert_eq!(location.thread_ts(), Some("1783500000.000001"));
    }

    #[test]
    fn slack_permalink_parser_rejects_external_and_malformed_links() {
        let workspace = Some("https://signicat.slack.com");
        for uri in [
            "https://other.slack.com/archives/C123/p1783592777735299",
            "https://example.com/archives/C123/p1783592777735299",
            "https://signicat.slack.com/client/C123/p1783592777735299",
            "https://signicat.slack.com/archives/C-123/p1783592777735299",
            "https://signicat.slack.com/archives/C123/p123",
            "https://signicat.slack.com/archives/C123/p17835927777oops",
            "https://signicat.slack.com/archives/C123/p1783592777735299?thread_ts=oops.bad",
            "https://signicat.slack.com/archives/C123/p1783592777735299/extra",
        ] {
            assert_eq!(slack_message_location(uri, workspace), None, "{uri}");
        }
    }

    #[test]
    fn generated_permalink_round_trips_to_internal_location() {
        let workspace = "https://signicat.slack.com";
        let uri = message_permalink(workspace, "C123", "1783592777.735299").unwrap();
        let location = slack_message_location(&uri, Some(workspace)).unwrap();
        assert_eq!(location.channel_id(), "C123");
        assert_eq!(location.message_ts(), "1783592777.735299");
    }

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
            search_aliases: Vec::new(),
            status: None,
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
            kind: RuntimeEventKind::RuntimeStartFailed(RuntimeFailure::validation(
                "runtime construction failed",
            )),
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
            kind: RuntimeEventKind::Error(RuntimeFailure::validation("stored token failed")),
        };
        assert!(!runtime_event_is_start_failure(&ordinary_error));
    }

    #[test]
    fn authentication_failures_always_recover_at_the_session_surface() {
        let failure = RuntimeFailure {
            category: RuntimeFailureCategory::Authentication,
            message: "Sign in again".to_string(),
        };
        let context = OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C123".to_string()),
        );

        assert_eq!(
            runtime_failure_recovery_for_failure(&context, &failure),
            RuntimeFailureRecovery::Session
        );
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
                RuntimeOperation::AttachmentDownload,
                RuntimeTarget::Attachment("https://files.slack.com/file.pdf".to_string()),
                RuntimeFailureRecovery::Attachment,
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
                RuntimeTarget::Upload {
                    channel_id: "C123".to_string(),
                    thread_ts: Some("1.0".to_string()),
                },
                RuntimeFailureRecovery::Upload {
                    channel_id: "C123".to_string(),
                    thread_ts: Some("1.0".to_string()),
                },
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
    fn status_expiration_scheduler_selects_nearest_future_expiration() {
        let statuses = HashMap::from([
            (
                "expired".to_string(),
                SlackUserStatus {
                    expiration: 90,
                    ..Default::default()
                },
            ),
            (
                "later".to_string(),
                SlackUserStatus {
                    expiration: 200,
                    ..Default::default()
                },
            ),
            (
                "next".to_string(),
                SlackUserStatus {
                    expiration: 150,
                    ..Default::default()
                },
            ),
        ]);

        assert_eq!(nearest_status_expiration(&statuses, 100), Some(150));
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
        assert!(features.media);
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
                previous_latest_ts: Some("1710000100.000000"),
                latest_ts: "1710000200.000000",
                latest_message_user: Some("U456"),
                current_user: Some("U123"),
                has_unread: true,
                muted: false,
                actively_reading: false,
                delivery: MessageNotificationDelivery::Snapshot,
            }),
            MessageNotificationAction::Notify
        );

        assert_eq!(
            message_notification_action(MessageNotificationState {
                previous_latest_ts: None,
                latest_ts: "1710000200.000000",
                latest_message_user: Some("U456"),
                current_user: Some("U123"),
                has_unread: true,
                muted: false,
                actively_reading: false,
                delivery: MessageNotificationDelivery::Realtime {
                    first_delivery: true,
                },
            }),
            MessageNotificationAction::Notify
        );
    }

    #[test]
    fn notification_policy_records_without_notifying_for_non_notifyable_messages() {
        let notifyable = MessageNotificationState {
            previous_latest_ts: Some("1710000100.000000"),
            latest_ts: "1710000200.000000",
            latest_message_user: Some("U456"),
            current_user: Some("U123"),
            has_unread: true,
            muted: false,
            actively_reading: false,
            delivery: MessageNotificationDelivery::Snapshot,
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
                actively_reading: true,
                ..notifyable
            }),
            MessageNotificationAction::RecordOnly
        );
        assert_eq!(
            message_notification_action(MessageNotificationState {
                previous_latest_ts: None,
                delivery: MessageNotificationDelivery::Realtime {
                    first_delivery: false,
                },
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
    fn notification_baseline_never_regresses_for_delayed_snapshots() {
        let baseline = notification_baseline_after(None, "1710000200.000000");
        assert_eq!(baseline, "1710000200.000000");

        let baseline = notification_baseline_after(Some(&baseline), "1710000100.000000");
        assert_eq!(baseline, "1710000200.000000");
        assert_eq!(
            message_notification_action(MessageNotificationState {
                previous_latest_ts: Some(&baseline),
                latest_ts: "1710000200.000000",
                latest_message_user: Some("U456"),
                current_user: Some("U123"),
                has_unread: true,
                muted: false,
                actively_reading: false,
                delivery: MessageNotificationDelivery::Snapshot,
            }),
            MessageNotificationAction::RecordOnly
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
            DraftKey::new("T123:U123", "C123", None),
            Some("comment".into())
        ));
        assert!(!record_upload_submission(
            &mut uploads,
            DraftKey::new("T123:U123", "C123", None),
            Some("replacement".into())
        ));
        assert!(record_upload_submission(
            &mut uploads,
            DraftKey::new("T123:U123", "C123", Some("1.0")),
            None
        ));
        assert!(record_upload_submission(
            &mut uploads,
            DraftKey::new("T123:U123", "C999", None),
            None
        ));
    }

    #[test]
    fn clipboard_image_detection_does_not_intercept_text_paste() {
        assert!(clipboard_mime_type_is_image("image/png"));
        assert!(clipboard_mime_type_is_image("image/jpeg; charset=binary"));
        assert!(!clipboard_mime_type_is_image("text/plain"));
        assert!(!clipboard_mime_type_is_image("application/pdf"));
    }

    #[test]
    fn sidebar_leave_action_is_only_available_for_active_channels() {
        let public_channel = SlackConversation {
            is_channel: Some(true),
            ..Default::default()
        };
        let private_channel = SlackConversation {
            is_private: Some(true),
            ..Default::default()
        };
        let direct_message = SlackConversation {
            is_im: Some(true),
            is_private: Some(true),
            ..Default::default()
        };
        let group_direct_message = SlackConversation {
            is_mpim: Some(true),
            is_group: Some(true),
            ..Default::default()
        };
        let archived_channel = SlackConversation {
            is_channel: Some(true),
            is_archived: Some(true),
            ..Default::default()
        };

        assert!(sidebar_conversation_can_leave(&public_channel));
        assert!(sidebar_conversation_can_leave(&private_channel));
        assert!(!sidebar_conversation_can_leave(&direct_message));
        assert!(!sidebar_conversation_can_leave(&group_direct_message));
        assert!(!sidebar_conversation_can_leave(&archived_channel));
        assert!(!sidebar_conversation_leave_requires_confirmation(
            &public_channel
        ));
        assert!(sidebar_conversation_leave_requires_confirmation(
            &private_channel
        ));
    }

    #[test]
    fn conversation_pane_image_paste_targets_the_originating_pane() {
        let control = gtk::gdk::ModifierType::CONTROL_MASK;
        assert_eq!(
            conversation_pane_image_paste_target(
                ConversationPanePasteFocus::MainPane,
                true,
                gtk::gdk::Key::v,
                control,
            ),
            Some(ComposerTarget::Message)
        );
        assert_eq!(
            conversation_pane_image_paste_target(
                ConversationPanePasteFocus::ThreadPane,
                true,
                gtk::gdk::Key::v,
                control,
            ),
            Some(ComposerTarget::Thread)
        );
    }

    #[test]
    fn conversation_pane_image_paste_excludes_inputs_and_unrelated_widgets() {
        let control = gtk::gdk::ModifierType::CONTROL_MASK;
        for focus in [
            ConversationPanePasteFocus::Composer,
            ConversationPanePasteFocus::TextInput,
            ConversationPanePasteFocus::Outside,
        ] {
            assert_eq!(
                conversation_pane_image_paste_target(focus, true, gtk::gdk::Key::v, control),
                None
            );
        }
    }

    #[test]
    fn conversation_pane_image_paste_preserves_normal_paste_shortcuts() {
        let control = gtk::gdk::ModifierType::CONTROL_MASK;
        let main = ConversationPanePasteFocus::MainPane;
        assert_eq!(
            conversation_pane_image_paste_target(main, false, gtk::gdk::Key::v, control),
            None
        );
        assert_eq!(
            conversation_pane_image_paste_target(
                main,
                true,
                gtk::gdk::Key::v,
                control | gtk::gdk::ModifierType::SHIFT_MASK,
            ),
            None
        );
        assert_eq!(
            conversation_pane_image_paste_target(main, true, gtk::gdk::Key::c, control,),
            None
        );
    }

    #[test]
    fn screenshot_staging_names_are_safe_png_files() {
        let first = screenshot_filename();

        assert!(first.starts_with("Screenshot-"));
        assert!(first.ends_with(".png"));
        assert!(!first.contains('/'));
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
            workspace_navigation_selection(MainMessageView::Threads),
            Some(WorkspaceNavigationSelection::Threads)
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
            MainMessageView::Threads,
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
            "GtkSeparator\" id=\"thread_resize_handle",
            "GtkBox\" id=\"message_pane",
            "GtkBox\" id=\"thread_pane",
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
            "GtkToggleButton\" id=\"threads_button",
            "<property name=\"group\">messages_button</property>",
            "<property name=\"icon-name\">view-list-symbolic</property>",
            "<property name=\"icon-name\">mail-unread-symbolic</property>",
            "<property name=\"tooltip-text\" translatable=\"yes\">Messages</property>",
            "<property name=\"tooltip-text\" translatable=\"yes\">Unreads</property>",
            "<property name=\"tooltip-text\" translatable=\"yes\">Threads</property>",
            "<property name=\"enable-show-gesture\">False</property>",
            "GtkLabel\" id=\"message_status_label",
            "<property name=\"accessible-role\">status</property>",
        ] {
            assert!(
                template.contains(required),
                "missing template marker {required}"
            );
        }

        assert!(template.contains("<property name=\"width-request\">10</property>"));
        assert!(template
            .contains("<property name=\"sidebar-width-fraction\">0.6666666666666666</property>"));
        assert!(template.contains("<property name=\"max-sidebar-width\">10000</property>"));
        assert!(!template.contains("<object class=\"GtkPaned\""));
        assert!(!template.contains("<property name=\"width-request\">460</property>"));
        assert!(!template.contains("<property name=\"width-request\">280</property>"));
        assert!(!template.contains("<property name=\"width-request\">220</property>"));
    }

    #[test]
    fn thread_sidebar_resize_follows_end_edge_and_clamps() {
        assert_eq!(
            resized_end_sidebar_fraction(400.0, -100.0, 1_000.0),
            Some(0.5)
        );
        assert_eq!(
            resized_end_sidebar_fraction(400.0, 100.0, 1_000.0),
            Some(0.3)
        );
        assert_eq!(
            resized_end_sidebar_fraction(400.0, -1_000.0, 1_000.0),
            Some(THREAD_PANE_MAX_FRACTION)
        );
        assert_eq!(
            resized_end_sidebar_fraction(400.0, 1_000.0, 1_000.0),
            Some(0.2)
        );
        assert_eq!(
            resized_end_sidebar_fraction(THREAD_PANE_MAX_FRACTION * 1_000.0, 0.0, 1_000.0,),
            Some(THREAD_PANE_MAX_FRACTION)
        );
        assert_eq!(resized_end_sidebar_fraction(400.0, 0.0, 0.0), None);
    }

    #[test]
    fn realtime_messages_stay_unread_until_visible_and_ignore_self_sent() {
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

        assert!(realtime_message_marks_unread(
            Some("C123"),
            true,
            Some("U999"),
            &event
        ));
        assert!(!realtime_message_marks_unread(
            None,
            true,
            Some("U123"),
            &event
        ));
        assert!(!realtime_message_marks_unread(
            None,
            true,
            Some("U999"),
            &changed
        ));
        assert!(realtime_message_marks_unread(
            Some("C999"),
            true,
            Some("U999"),
            &event
        ));
        assert!(realtime_message_marks_unread(
            Some("C123"),
            false,
            Some("U999"),
            &event
        ));
    }

    #[test]
    fn realtime_dom_posts_append_only_when_they_are_newest() {
        let existing = [
            SlackMessage {
                ts: "3".to_string(),
                ..Default::default()
            },
            SlackMessage {
                ts: "1".to_string(),
                ..Default::default()
            },
        ];

        assert_eq!(
            realtime_dom_patch_kind(
                RealtimeMessageKind::Posted,
                &existing,
                &SlackMessage {
                    ts: "4".to_string(),
                    ..Default::default()
                }
            ),
            Some(RealtimeMessageKind::Posted)
        );
        assert_eq!(
            realtime_dom_patch_kind(
                RealtimeMessageKind::Posted,
                &existing,
                &SlackMessage {
                    ts: "2".to_string(),
                    ..Default::default()
                }
            ),
            None
        );
    }

    #[test]
    fn realtime_dom_redeliveries_replace_instead_of_duplicate() {
        let existing = [SlackMessage {
            ts: "3".to_string(),
            ..Default::default()
        }];
        let redelivery = SlackMessage {
            ts: "3".to_string(),
            ..Default::default()
        };

        assert_eq!(
            realtime_dom_patch_kind(RealtimeMessageKind::Posted, &existing, &redelivery),
            Some(RealtimeMessageKind::Changed)
        );
        assert_eq!(
            realtime_dom_patch_kind(RealtimeMessageKind::Deleted, &existing, &redelivery),
            Some(RealtimeMessageKind::Deleted)
        );
    }

    #[test]
    fn unread_focus_starts_after_last_read_or_uses_unread_count() {
        let messages = [
            SlackMessage {
                ts: "3".to_string(),
                ..Default::default()
            },
            SlackMessage {
                ts: "1".to_string(),
                ..Default::default()
            },
            SlackMessage {
                ts: "2".to_string(),
                ..Default::default()
            },
        ];

        assert_eq!(
            first_unread_message_ts(&messages, Some("1"), 0).as_deref(),
            Some("2")
        );
        assert_eq!(
            first_unread_message_ts(&messages, None, 2).as_deref(),
            Some("2")
        );
        assert_eq!(first_unread_message_ts(&messages, None, 0), None);
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

    #[test]
    fn emoji_completion_is_wired_to_both_composers() {
        assert_eq!(
            COMPOSER_TARGETS,
            [ComposerTarget::Message, ComposerTarget::Thread]
        );
    }
}
