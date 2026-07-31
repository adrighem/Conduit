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
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::activity::{self, ActivityItem};
use crate::attention::AttentionDecision;
use crate::attention_settings;
use crate::auth;
use crate::composer::{
    completion_key_action, emoji_token_at_caret, hydrate_composer_mentions, mention_candidates,
    mention_token_at_caret, replace_emoji_token, replace_mention_token, search_mention_candidates,
    serialize_composer_mentions, text_view_enter_action, text_view_text, CompletionKeyAction,
    EmojiToken, MentionCandidate, MentionSpan, MentionToken, TextViewEnterAction,
};
use crate::config;
use crate::drafts::{DraftKey, DraftSettings, Drafts};
use crate::emoji::{
    emoji_picker_accessible_label, move_emoji_picker_selection, EmojiCatalog, EmojiEntry,
    EmojiPickerGenerationGate, EmojiPickerModel, EmojiPickerMove, EmojiPickerQuery, EmojiValue,
};
use crate::huddles::fallback::external_huddle_url;
use crate::huddles::presentation::{present_huddle, HuddlePrimaryAction};
use crate::huddles::state::{
    HuddleCommand, HuddleDevice, HuddleDeviceKind, HuddleEvent, HuddlePhase,
    HuddleScreenShareState, HuddleSnapshot,
};
use crate::message_handoff::{
    open_resolved_handoff, ExternalOpenError, ExternalOpener, HandoffProvenance,
    MessageControlRegistry, MessageRef, SafeSlackPermalink, TimelineSurfaceId,
};
use crate::message_html::{
    self, MessageHtmlContext, TimelineDomPatch, TimelineInsertPosition, TimelineMessageArrival,
    TimelineMessageRegion, TimelineScrollBehavior,
};
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation, SlackFile,
    SlackMessage, SlackUser, SlackUserProfile, SlackUserStatus,
};
use crate::realtime::{RealtimePhase, RealtimeStatus};
use crate::rendering;
use crate::runtime::{
    AppRuntime, OperationContext, RequestId, RuntimeCommand, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimeFailure, RuntimeFailureCategory, RuntimeIdentity, RuntimeOperation,
    RuntimeTarget, SessionId,
};
use crate::shortcuts::WINDOW_SHORTCUTS;
use crate::sidebar::{
    self, ConversationKind, ConversationPickerAction, ConversationPickerItem,
    ConversationPickerSections, KeyedSidebarItem, SidebarItemKey, SidebarItemModel,
    SidebarProjection, SidebarProjectionError, SidebarProjectionOperation, SidebarRowModel,
    SidebarSectionKind,
};
use crate::sidebar_widgets::{sidebar_row_widget, SidebarRowLayout};
use crate::slack_link::{
    resolve_slack_uri, slack_app_web_fallback, SlackFileAction, SlackUri, SlackUriResolution,
    SlackUriTarget,
};
use crate::socket_mode::{
    SocketModeEvent, SocketModeMessageEvent, SocketModeMessageKind, SocketModeReactionEvent,
};
use crate::thread_pane::ThreadPane;
use crate::workspace_state::{
    resolve_first_unread_message_ts, ConversationOpenCoordinator, ConversationOpenIntent,
    ConversationOpenPosition, ConversationOpenRenderAction, ConversationSelectionDecision,
    MainMessageView, ReactionUpdate, RealtimeMessageKind, RealtimeMessageOutcome,
    ThreadApplyOutcome, ThreadOpenOutcome, WorkspaceLifecycle, WorkspaceLifecycleEvent,
    WorkspacePatchRemoval, WorkspaceScrollBehavior, WorkspaceSessionState, WorkspaceSnapshot,
};

#[derive(Debug, Clone)]
struct HuddleDevicePicker {
    dropdown: gtk::DropDown,
    ids: Rc<RefCell<Vec<String>>>,
    updating: Rc<Cell<bool>>,
}

#[derive(Debug, Clone)]
struct HuddlePreflightDialog {
    dialog: adw::AlertDialog,
    microphone: HuddleDevicePicker,
    speaker: HuddleDevicePicker,
    camera: HuddleDevicePicker,
}

#[derive(Debug, Clone)]
struct StatusDialogState {
    dialog: adw::AlertDialog,
    status_entry: adw::EntryRow,
    emoji_picker: StatusEmojiPicker,
    expiration_choice_count: usize,
}

#[derive(Debug, Clone)]
struct PendingStatusUpdate {
    requested: SlackUserStatus,
    dialog_draft: SlackUserStatus,
    clearing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusExpirationChoice {
    Never,
    Minutes30,
    Hour1,
    Hours4,
    Today,
    ThisWeek,
    Existing(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserStatusPresentation {
    subtitle: String,
    accessible_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusEmojiChoice {
    name: String,
    label: String,
}

#[derive(Debug, Clone)]
struct StatusEmojiPickerModel {
    emojis: EmojiPickerModel,
}

#[derive(Debug, Clone)]
struct StatusEmojiPicker {
    row: adw::ActionRow,
    popover: gtk::Popover,
    search: gtk::SearchEntry,
    visible_model: gtk::StringList,
    visible_choices: Rc<RefCell<Vec<StatusEmojiChoice>>>,
    selection: gtk::SingleSelection,
    source: Rc<RefCell<StatusEmojiPickerModel>>,
    selected_name: Rc<RefCell<String>>,
}

#[derive(Debug, Clone)]
struct ComposerMentionMark {
    start: gtk::TextMark,
    end: gtk::TextMark,
    user_id: String,
    label: String,
}

struct SystemExternalOpener;

impl ExternalOpener for SystemExternalOpener {
    fn open(&self, permalink: &SafeSlackPermalink) -> Result<(), ExternalOpenError> {
        open::that(permalink.as_str()).map_err(|error| ExternalOpenError::new(error.to_string()))
    }
}

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
        pub browser_session_howto_link: TemplateChild<gtk::LinkButton>,
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
        pub sidebar_all_filter_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub conversation_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub workspace_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub connection_status_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub message_status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub message_title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub navigation_back_button: TemplateChild<gtk::Button>,
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
        pub huddle_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub huddle_title_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub huddle_detail_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub huddle_primary_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_external_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_controls_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub huddle_mute_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_camera_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_share_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_leave_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub huddle_dismiss_button: TemplateChild<gtk::Button>,
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
        pub(super) message_control_registry: RefCell<MessageControlRegistry>,
        pub(super) conversation_opening: RefCell<ConversationOpenCoordinator>,
        pub settings: RefCell<Option<gio::Settings>>,
        pub connect_requested: Cell<bool>,
        pub auth_debug: Cell<bool>,
        pub(super) workspace: WorkspaceSessionState,
        pub discovered_channels: RefCell<Vec<SlackConversation>>,
        pub discovered_users: RefCell<Vec<SlackUser>>,
        pub user_directory_loaded: Cell<bool>,
        pub user_directory_failed: Cell<bool>,
        pub(super) conversation_picker_view: RefCell<Option<ConversationPickerView>>,
        pub(super) people_picker_view: RefCell<Option<PeoplePickerView>>,
        pub(super) sidebar_row_actions: RefCell<HashMap<i32, SidebarRowAction>>,
        pub(super) sidebar_section_actions: RefCell<HashMap<i32, SidebarSectionKind>>,
        pub(super) collapsed_sidebar_sections: RefCell<HashSet<SidebarSectionKind>>,
        pub user_names: RefCell<Arc<HashMap<String, String>>>,
        pub user_full_names: RefCell<Arc<HashMap<String, String>>>,
        pub user_avatar_urls: RefCell<Arc<HashMap<String, String>>>,
        pub user_search_aliases: RefCell<sidebar::UserSearchAliases>,
        pub user_statuses: RefCell<Arc<sidebar::UserStatuses>>,
        pub status_expiry_generation: Cell<u64>,
        pub user_group_names: RefCell<Arc<HashMap<String, String>>>,
        pub user_group_members: RefCell<Arc<HashMap<String, Vec<String>>>>,
        pub pending_user_ids: RefCell<HashSet<String>>,
        pub deferred_user_ids: RefCell<HashSet<String>>,
        pub user_directory_request_in_flight: Cell<bool>,
        pub pending_profile_user_id: RefCell<Option<String>>,
        pub workspace_id: RefCell<Option<String>>,
        pub workspace_team_id: RefCell<Option<String>>,
        pub workspace_name: RefCell<Option<String>>,
        pub workspace_url: RefCell<Option<String>>,
        pub workspace_ready: Cell<bool>,
        // Cached data can make the workspace ready for routing before the initial live sync ends.
        pub initial_sync_complete: Cell<bool>,
        pub(super) membership_reactivation: RefCell<MembershipReactivationState>,
        pub(super) pending_notification_target: RefCell<Option<NotificationTarget>>,
        pub(super) pending_message_notifications:
            RefCell<HashMap<(String, String), PendingMessageNotification>>,
        pub(super) pending_slack_uris: RefCell<VecDeque<SlackUri>>,
        pub(super) huddle_snapshot: RefCell<HuddleSnapshot>,
        pub(super) huddle_devices: RefCell<Vec<HuddleDevice>>,
        pub(super) huddle_preflight_dialog: RefCell<Option<HuddlePreflightDialog>>,
        pub(super) status_dialog: RefCell<Option<StatusDialogState>>,
        pub(super) pending_status_update: RefCell<Option<PendingStatusUpdate>>,
        pub(super) notified_huddle_call_id: RefCell<Option<String>>,
        pub drafts: RefCell<Drafts>,
        pub draft_save_generation: Cell<u64>,
        pub draft_persist_pending: Cell<bool>,
        pub pending_sent_drafts: RefCell<HashMap<DraftKey, String>>,
        pub pending_upload_drafts: RefCell<HashMap<DraftKey, Option<String>>>,
        pub sidebar_error: RefCell<Option<String>>,
        pub current_user_id: RefCell<Option<String>>,
        pub message_view: RefCell<Option<webkit6::WebView>>,
        pub message_font_settings_handler: RefCell<Option<(gtk::Settings, glib::SignalHandlerId)>>,
        pub(super) media_viewer: RefCell<Option<MediaViewer>>,
        pub(super) thread_pane_controller: RefCell<Option<ThreadPane>>,
        pub image_assets: RefCell<HashMap<String, String>>,
        pub pending_image_assets: RefCell<HashSet<String>>,
        pub failed_image_assets: RefCell<HashSet<String>>,
        pub custom_emojis: RefCell<Arc<HashMap<String, String>>>,
        pub reaction_emoji_picker_model: RefCell<Option<Arc<EmojiPickerModel>>>,
        pub realtime_status: Cell<RealtimeStatus>,
        pub(super) message_composer_completion: RefCell<Option<ComposerCompletion>>,
        pub(super) thread_composer_completion: RefCell<Option<ComposerCompletion>>,
        pub(super) message_mentions: RefCell<Vec<ComposerMentionMark>>,
        pub(super) thread_mentions: RefCell<Vec<ComposerMentionMark>>,
        pub(super) pending_ui_invalidations: Cell<UiInvalidations>,
        pub(super) sidebar_projection: RefCell<SidebarProjection>,
        pub(super) sidebar_rows: RefCell<HashMap<SidebarItemKey, gtk::ListBoxRow>>,
        pub(super) sidebar_filter_generation: Cell<u64>,
        pub(super) picker_filter_generation: Cell<u64>,
        pub(super) picker_population_generation: Cell<u64>,
        pub(super) navigation_history: RefCell<Vec<MainNavigationTarget>>,
        pub(super) restoring_navigation: Cell<bool>,
        pub(super) profile_visible: Cell<bool>,
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
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
                vec![glib::subclass::Signal::builder("realtime-status-changed").build()]
            });
            SIGNALS.as_ref()
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_adaptive_layout();
            obj.setup_runtime();
            obj.setup_message_view();
            obj.setup_reaction_picker_escape_fallback();
            obj.configure_accessibility();
            obj.configure_auth_ui();
            obj.setup_settings();
            obj.setup_callbacks();
            if std::env::var_os("CONDUIT_TEST_WORKSPACE").is_some() {
                let huddle_test = std::env::var_os("CONDUIT_TEST_HUDDLE").is_some();
                let status_test = std::env::var_os("CONDUIT_TEST_STATUS_DIALOG").is_some();
                let initial_sync_test = std::env::var_os("CONDUIT_TEST_INITIAL_SYNC").is_some();
                if status_test && std::env::var_os("CONDUIT_TEST_STATUS_NARROW").is_some() {
                    obj.set_default_size(360, 720);
                }
                let test_channel_id = if huddle_test { "CTEST" } else { "C_TEST" };
                obj.apply_workspace_lifecycle(WorkspaceLifecycleEvent::ConnectRequested);
                obj.apply_workspace_lifecycle(WorkspaceLifecycleEvent::Authenticated);
                obj.show_workspace(AuthInfo {
                    team: Some("Test Workspace".to_string()),
                    team_id: huddle_test.then(|| "TTEST".to_string()),
                    user: (huddle_test || status_test).then(|| "Test User".to_string()),
                    user_id: (huddle_test || status_test).then(|| "UTEST".to_string()),
                    ..AuthInfo::default()
                });
                if initial_sync_test {
                    return;
                }
                let patch = crate::workspace_pipeline::WorkspacePatch::new(
                    crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
                    vec![
                        crate::workspace_pipeline::WorkspaceChange::ConversationsReset(vec![
                            SlackConversation {
                                id: test_channel_id.to_string(),
                                name: Some("general".to_string()),
                                is_channel: Some(true),
                                ..SlackConversation::default()
                            },
                        ]),
                    ],
                )
                .expect("test workspace patch must contain its conversation");
                obj.apply_workspace_patch(&patch);
                let test_users = vec![
                    SlackUser {
                        id: Some("UADA".to_string()),
                        name: Some("ada".to_string()),
                        real_name: Some("Ada Lovelace".to_string()),
                        profile: Some(crate::models::SlackUserProfile {
                            display_name: Some("Ada Lovelace".to_string()),
                            real_name: Some("Ada Lovelace".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    SlackUser {
                        id: Some("UGRACE".to_string()),
                        name: Some("grace".to_string()),
                        real_name: Some("Grace Hopper".to_string()),
                        profile: Some(crate::models::SlackUserProfile {
                            display_name: Some("Grace Hopper".to_string()),
                            real_name: Some("Grace Hopper".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ];
                obj.populate_user_names(
                    test_users
                        .iter()
                        .filter_map(|user| Some((user.id.clone()?, user.display_name()?)))
                        .collect(),
                );
                *self.discovered_users.borrow_mut() = test_users;
                if status_test && std::env::var_os("CONDUIT_TEST_STATUS_PRESET").is_some() {
                    Arc::make_mut(&mut self.user_statuses.borrow_mut()).insert(
                        "UTEST".to_string(),
                        SlackUserStatus {
                            text: "Working remotely".to_string(),
                            emoji: ":house:".to_string(),
                            expiration: 0,
                        },
                    );
                    obj.refresh_workspace_title_status();
                }
                obj.apply_workspace_lifecycle(WorkspaceLifecycleEvent::SyncCompleted);
                obj.select_conversation(test_channel_id, "#general");
                if huddle_test {
                    let huddle = crate::huddles::model::ActiveHuddle {
                        team_id: "TTEST".to_string(),
                        channel_id: test_channel_id.to_string(),
                        call_id: "RTEST".to_string(),
                        name: Some("Test huddle".to_string()),
                        participant_ids: vec!["UTEST".to_string()],
                        started_at: None,
                        huddle_link: None,
                    };
                    obj.handle_huddle_event(HuddleEvent::Snapshot(Box::new(HuddleSnapshot {
                        phase: HuddlePhase::Discovered,
                        huddle: Some(huddle),
                        participants: vec![crate::huddles::state::HuddleParticipant::from_user_id(
                            "UTEST".to_string(),
                        )],
                        ..Default::default()
                    })));
                }
                if std::env::var_os("CONDUIT_TEST_THREAD_COMPOSER").is_some() {
                    obj.imp().thread_split.set_show_sidebar(true);
                }
                if std::env::var_os("CONDUIT_TEST_COMPOSER_HYDRATION").is_some() {
                    let target = if std::env::var_os("CONDUIT_TEST_THREAD_COMPOSER").is_some() {
                        ComposerTarget::Thread
                    } else {
                        ComposerTarget::Message
                    };
                    Arc::make_mut(&mut obj.imp().user_names.borrow_mut()).remove("UGRACE");
                    obj.set_composer_canonical_text(target, "Draft <@UGRACE>");
                    obj.populate_user_names(HashMap::from([(
                        "UGRACE".to_string(),
                        "Grace Hopper".to_string(),
                    )]));
                }
                if status_test {
                    let weak_window = obj.downgrade();
                    glib::idle_add_local_once(move || {
                        if let Some(window) = weak_window.upgrade() {
                            let _ = gtk::prelude::WidgetExt::activate_action(
                                &window,
                                "win.change-status",
                                None,
                            );
                        }
                    });
                    if std::env::var_os("CONDUIT_TEST_STATUS_LATE_EMOJI").is_some() {
                        let weak_window = obj.downgrade();
                        glib::timeout_add_local_once(Duration::from_millis(100), move || {
                            if let Some(window) = weak_window.upgrade() {
                                window.replace_custom_emojis(HashMap::from([(
                                    "late_status_parrot".to_string(),
                                    "https://emoji.example/late-status-parrot.gif".to_string(),
                                )]));
                            }
                        });
                    }
                }
            } else {
                obj.show_loading("Checking secure storage");
                obj.send_session_command(RuntimeCommand::LoadStoredToken);
            }
        }

        fn dispose(&self) {
            if let Some((settings, handler)) =
                self.message_font_settings_handler.borrow_mut().take()
            {
                settings.disconnect(handler);
            }

            // These popovers are manually parented to GtkTextView so they can
            // point at the composer caret. Detach them before the template
            // children are disposed; GtkTextView cannot remove unregistered
            // direct children itself and otherwise loops while warning.
            for completion in [
                &self.message_composer_completion,
                &self.thread_composer_completion,
            ] {
                if let Some(completion) = completion.borrow_mut().take() {
                    completion.popover.popdown();
                    if completion.popover.parent().is_some() {
                        completion.popover.unparent();
                    }
                }
            }
            let status_dialog = self.status_dialog.borrow_mut().take();
            if let Some(state) = status_dialog {
                state.dialog.force_close();
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

struct RealtimeMessagePatch<'a> {
    surface: TimelineSurface,
    channel_id: &'a str,
    message: &'a SlackMessage,
    kind: RealtimeMessageKind,
    arrival: Option<TimelineMessageArrival>,
    unread_start: bool,
    thread_ts: Option<&'a str>,
    fallback: UiInvalidations,
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
const MAX_PENDING_SLACK_URIS: usize = 16;
const MAX_PENDING_MESSAGE_NOTIFICATIONS: usize = 128;
const PICKER_POPULATION_BATCH_SIZE: usize = 24;
const EMOJI_PICKER_MESSAGE_HANDLER: &str = "conduitEmojiPicker";
const APPLY_EMOJI_PICKER_RESULT_SCRIPT: &str =
    "window.conduitReceiveEmojiPickerResult(JSON.parse(payload));";
const CANCEL_REACTION_PICKER_SCRIPT: &str = r#"(function () {
  const picker = document.getElementById("emoji-picker");
  if (!picker || !picker.open) return false;
  picker.dispatchEvent(new Event("cancel", { cancelable: true }));
  return true;
})()"#;

#[derive(Debug, Clone)]
enum ComposerCompletionToken {
    Emoji(EmojiToken),
    Mention(MentionToken),
}

#[derive(Debug, Clone)]
enum ComposerCompletionEntry {
    Emoji(EmojiEntry),
    Mention(MentionCandidate),
}

#[derive(Debug)]
struct ComposerCompletion {
    popover: gtk::Popover,
    list: gtk::ListBox,
    entries: Vec<ComposerCompletionEntry>,
    token: Option<ComposerCompletionToken>,
    directory_checked_mention_start: Option<usize>,
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

fn composer_person_detail(candidate: &MentionCandidate) -> Option<String> {
    let mut details = Vec::new();
    if let Some(full_name) = candidate
        .full_name
        .as_deref()
        .filter(|name| !name.eq_ignore_ascii_case(&candidate.display_name))
    {
        details.push(full_name.to_string());
    }
    if let Some(username) = candidate
        .username
        .as_deref()
        .filter(|name| !name.eq_ignore_ascii_case(&candidate.display_name))
    {
        details.push(format!("@{username}"));
    }
    (!details.is_empty()).then(|| details.join("  "))
}

fn composer_completion_row(entry: &ComposerCompletionEntry) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(8);
    content.set_margin_end(8);

    match entry {
        ComposerCompletionEntry::Emoji(entry) => {
            let preview = composer_emoji_preview(entry);
            let label = gtk::Label::new(Some(&format!(":{}:  {}", entry.name, entry.label)));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            content.append(&preview);
            content.append(&label);
            row.update_property(&[gtk::accessible::Property::Label(
                &emoji_picker_accessible_label(entry),
            )]);
        }
        ComposerCompletionEntry::Mention(candidate) => {
            let preview = gtk::Image::from_icon_name("avatar-default-symbolic");
            preview.set_pixel_size(24);

            let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
            labels.set_hexpand(true);
            let primary = gtk::Label::new(Some(&format!("@{}", candidate.display_name)));
            primary.set_xalign(0.0);
            labels.append(&primary);
            let detail = composer_person_detail(candidate);
            if let Some(detail) = detail.as_deref() {
                let secondary = gtk::Label::new(Some(detail));
                secondary.add_css_class("dim-label");
                secondary.set_xalign(0.0);
                labels.append(&secondary);
            }

            content.append(&preview);
            content.append(&labels);
            let accessible = detail.map_or_else(
                || format!("Person: {}", candidate.display_name),
                |detail| {
                    format!(
                        "Person: {}, {}",
                        candidate.display_name,
                        detail.replace("  ", ", ")
                    )
                },
            );
            row.update_property(&[gtk::accessible::Property::Label(&accessible)]);
        }
    }

    row.set_child(Some(&content));
    row
}

fn composer_completion_description(
    entry: &ComposerCompletionEntry,
    index: usize,
    total: usize,
) -> String {
    let position = format!("{} of {}", index + 1, total);
    match entry {
        ComposerCompletionEntry::Emoji(entry) => {
            format!(
                "Emoji suggestion {position}: :{}:, {}",
                entry.name, entry.label
            )
        }
        ComposerCompletionEntry::Mention(candidate) => {
            let detail = composer_person_detail(candidate)
                .map(|detail| format!(", {}", detail.replace("  ", ", ")))
                .unwrap_or_default();
            format!(
                "Person suggestion {position}: {}{detail}",
                candidate.display_name
            )
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

type PeoplePickerSubmit = Rc<dyn Fn(&ConduitWindow, Vec<String>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeoplePickerCandidate {
    user_id: String,
    name: String,
}

#[derive(Debug, Clone)]
struct PeoplePickerRow {
    user_id: String,
    search_name: String,
    check: gtk::CheckButton,
    row: gtk::ListBoxRow,
}

#[derive(Clone)]
struct PeoplePickerView {
    dialog: gtk::Window,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    confirm: gtk::Button,
    rows: Rc<RefCell<Vec<PeoplePickerRow>>>,
    excluded_user_ids: HashSet<String>,
}

impl std::fmt::Debug for PeoplePickerView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PeoplePickerView")
            .field("dialog", &self.dialog)
            .field("search", &self.search)
            .field("list", &self.list)
            .field("confirm", &self.confirm)
            .field("rows", &self.rows)
            .field("excluded_user_ids", &self.excluded_user_ids)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationPickerListEntry {
    Header(String),
    Item(ConversationPickerItem),
    Placeholder(String),
}

#[derive(Debug)]
struct ConversationPickerPopulation {
    generation: u64,
    entries: VecDeque<ConversationPickerListEntry>,
}

impl ConversationPickerPopulation {
    fn new(generation: u64, entries: VecDeque<ConversationPickerListEntry>) -> Self {
        Self {
            generation,
            entries,
        }
    }

    fn next_batch(&mut self, current_generation: u64) -> Option<Vec<ConversationPickerListEntry>> {
        if self.generation != current_generation {
            self.entries.clear();
            return None;
        }
        if self.entries.is_empty() {
            return None;
        }
        let batch_size = self.entries.len().min(PICKER_POPULATION_BATCH_SIZE);
        Some(self.entries.drain(..batch_size).collect())
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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

fn sidebar_section_accessible_label(title: &str, collapsed: bool) -> String {
    format!(
        "{} {title}",
        if collapsed {
            gettext("Expand")
        } else {
            gettext("Collapse")
        }
    )
}

fn toggle_sidebar_section_state(
    collapsed_sections: &mut HashSet<SidebarSectionKind>,
    section: SidebarSectionKind,
) {
    if !collapsed_sections.insert(section) {
        collapsed_sections.remove(&section);
    }
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

fn people_picker_candidates(
    users: &[SlackUser],
    current_user_id: Option<&str>,
    excluded_user_ids: &HashSet<String>,
) -> Vec<PeoplePickerCandidate> {
    let mut people = users
        .iter()
        .filter(|user| !user.deleted.unwrap_or(false) && !user.is_bot.unwrap_or(false))
        .filter_map(|user| {
            let user_id = user.id.as_deref()?.trim();
            let name = user.direct_message_name()?;
            (!user_id.is_empty()
                && Some(user_id) != current_user_id
                && !excluded_user_ids.contains(user_id))
            .then(|| PeoplePickerCandidate {
                user_id: user_id.to_string(),
                name,
            })
        })
        .collect::<Vec<_>>();
    people.sort_by_key(|person| person.name.to_lowercase());
    people
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserLookupPlan {
    deferred_ids: Vec<String>,
    individual_ids: Vec<String>,
    request_directory: bool,
}

fn plan_user_id_lookups(
    ids: Vec<String>,
    known_user_ids: &HashSet<String>,
    directory_user_ids: &HashSet<String>,
    pending_user_ids: &HashSet<String>,
    directory_loaded: bool,
    directory_failed: bool,
    directory_request_in_flight: bool,
) -> UserLookupPlan {
    let mut missing_ids = ids
        .into_iter()
        .map(|user_id| user_id.trim().to_string())
        .filter(|user_id| {
            !user_id.is_empty()
                && !known_user_ids.contains(user_id)
                && !pending_user_ids.contains(user_id)
        })
        .collect::<Vec<_>>();
    missing_ids.sort();
    missing_ids.dedup();
    if directory_loaded {
        missing_ids.retain(|user_id| !directory_user_ids.contains(user_id));
    }

    if !directory_loaded || directory_failed || directory_request_in_flight {
        return UserLookupPlan {
            request_directory: !missing_ids.is_empty()
                && !directory_loaded
                && !directory_failed
                && !directory_request_in_flight,
            deferred_ids: missing_ids,
            individual_ids: Vec::new(),
        };
    }

    UserLookupPlan {
        deferred_ids: Vec::new(),
        individual_ids: missing_ids,
        request_directory: false,
    }
}

fn discovered_user_ids(users: &[SlackUser]) -> HashSet<String> {
    users
        .iter()
        .filter_map(|user| user.id.as_deref())
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct UserDirectoryIdentityMaps {
    names: HashMap<String, String>,
    full_names: HashMap<String, String>,
    avatar_urls: HashMap<String, String>,
}

fn user_directory_identity_maps(users: &[SlackUser]) -> UserDirectoryIdentityMaps {
    let mut maps = UserDirectoryIdentityMaps::default();
    for user in users {
        let Some(user_id) = user
            .id
            .as_deref()
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())
        else {
            continue;
        };
        if let Some(name) = user.display_name().filter(|name| !name.trim().is_empty()) {
            maps.names.insert(user_id.to_string(), name);
        }
        if let Some(full_name) = user.full_name().filter(|name| !name.trim().is_empty()) {
            maps.full_names.insert(user_id.to_string(), full_name);
        }
        if let Some(avatar_url) = user.avatar_url().filter(|url| !url.trim().is_empty()) {
            maps.avatar_urls.insert(user_id.to_string(), avatar_url);
        }
    }
    maps
}

fn changed_map_keys<T: PartialEq>(
    current: &HashMap<String, T>,
    replacement: &HashMap<String, T>,
) -> Vec<String> {
    let mut keys = current
        .keys()
        .chain(replacement.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| current.get(key) != replacement.get(key))
        .collect()
}

fn is_user_directory_request(context: &OperationContext) -> bool {
    context.operation == RuntimeOperation::User && context.target == RuntimeTarget::Workspace
}

fn merge_discovered_user_update(users: &mut Vec<SlackUser>, mut update: SlackUser) -> bool {
    let Some(user_id) = update
        .id
        .as_deref()
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(ToString::to_string)
    else {
        return false;
    };
    update.id = Some(user_id.clone());

    let Some(existing) = users.iter_mut().find(|user| {
        user.id
            .as_deref()
            .is_some_and(|existing_id| existing_id.trim() == user_id)
    }) else {
        users.push(update);
        return true;
    };
    let merged = existing.clone().merge_sparse_update(update);
    if *existing == merged {
        return false;
    }
    *existing = merged;
    true
}

fn composer_directory_check_transition(
    mention_start: Option<usize>,
    checked_mention_start: Option<usize>,
) -> (Option<usize>, bool) {
    (
        mention_start,
        mention_start.is_some() && mention_start != checked_mention_start,
    )
}

fn failed_user_directory_request(context: &OperationContext, directory_empty: bool) -> bool {
    directory_empty && is_user_directory_request(context)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeoplePickerDirectoryState {
    Loading,
    LoadedEmpty,
    Failed,
}

fn people_picker_directory_state(
    directory_loaded: bool,
    directory_failed: bool,
) -> PeoplePickerDirectoryState {
    if directory_failed {
        PeoplePickerDirectoryState::Failed
    } else if directory_loaded {
        PeoplePickerDirectoryState::LoadedEmpty
    } else {
        PeoplePickerDirectoryState::Loading
    }
}

fn people_picker_empty_message(state: PeoplePickerDirectoryState) -> &'static str {
    match state {
        PeoplePickerDirectoryState::Loading => "Loading people...",
        PeoplePickerDirectoryState::LoadedEmpty => "No people available",
        PeoplePickerDirectoryState::Failed => "Could not load people",
    }
}

fn conversation_picker_population_entries(
    sections: &ConversationPickerSections,
) -> VecDeque<ConversationPickerListEntry> {
    let mut entries = VecDeque::new();
    if let Some(results) = sections.search_results.as_deref() {
        entries.extend(
            results
                .iter()
                .cloned()
                .map(ConversationPickerListEntry::Item),
        );
    } else {
        for (title, items) in [
            ("Conversations", sections.conversations.as_slice()),
            ("Channels you can join", sections.channels.as_slice()),
            ("People", sections.people.as_slice()),
        ] {
            if items.is_empty() {
                continue;
            }
            entries.push_back(ConversationPickerListEntry::Header(title.to_string()));
            entries.extend(items.iter().cloned().map(ConversationPickerListEntry::Item));
        }
    }
    if entries.is_empty() {
        entries.push_back(ConversationPickerListEntry::Placeholder(gettext(
            "No matching conversations",
        )));
    }
    entries
}

fn valid_channel_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 80
        && name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum MainNavigationTarget {
    Conversation(String),
    Unreads,
    Threads,
    Search,
    Files,
    Saved,
}

const MAX_NAVIGATION_HISTORY: usize = 100;

fn remember_navigation(
    history: &mut Vec<MainNavigationTarget>,
    current: MainNavigationTarget,
    target: &MainNavigationTarget,
) {
    if &current == target || history.last() == Some(&current) {
        return;
    }
    history.push(current);
    if history.len() > MAX_NAVIGATION_HISTORY {
        history.remove(0);
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarConversationStarAction {
    starred: bool,
}

impl SidebarConversationStarAction {
    fn label(self) -> &'static str {
        if self.starred {
            "Star"
        } else {
            "Unstar"
        }
    }
}

fn sidebar_conversation_star_action(
    conversation: &SlackConversation,
) -> Option<SidebarConversationStarAction> {
    matches!(
        sidebar::conversation_kind(conversation),
        ConversationKind::PublicChannel
            | ConversationKind::PrivateChannel
            | ConversationKind::DirectMessage
            | ConversationKind::GroupDirectMessage
    )
    .then_some(SidebarConversationStarAction {
        starred: !conversation.is_starred(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarConversationProfileAction {
    user_id: String,
}

impl SidebarConversationProfileAction {
    fn label(&self) -> &'static str {
        "Profile"
    }
}

fn sidebar_conversation_profile_action(
    conversation: &SlackConversation,
) -> Option<SidebarConversationProfileAction> {
    (sidebar::conversation_kind(conversation) == ConversationKind::DirectMessage)
        .then_some(conversation.user.as_deref())
        .flatten()
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(|user_id| SidebarConversationProfileAction {
            user_id: user_id.to_string(),
        })
}

fn sidebar_context_menu_key(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    key == gtk::gdk::Key::Menu
        || (key == gtk::gdk::Key::F10 && state.contains(gtk::gdk::ModifierType::SHIFT_MASK))
}

fn conversation_sync_completion_needs_catalog_sync(workspace_ready: bool) -> bool {
    !workspace_ready
}

fn remove_patch_departures_from_discovery(
    discovered: &mut Vec<SlackConversation>,
    removals: &[WorkspacePatchRemoval],
) {
    for removal in removals {
        if removal
            .conversation()
            .is_some_and(sidebar_conversation_leave_requires_confirmation)
        {
            discovered.retain(|conversation| conversation.id != removal.channel_id());
        }
    }
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

fn message_notification_body(
    message: Option<&SlackMessage>,
    user_names: &HashMap<String, String>,
) -> Option<String> {
    let visible_text = message.map(SlackMessage::visible_text).unwrap_or_default();
    let text = visible_text.trim();
    if text.is_empty() {
        Some(gettext("New message"))
    } else {
        rendering::resolve_user_mentions(text, user_names)
    }
}

fn message_notification_content(
    conversation_title: &str,
    channel_notification: bool,
    message: &SlackMessage,
    user_names: &HashMap<String, String>,
) -> Option<(String, String)> {
    let body = message_notification_body(Some(message), user_names)?;
    if !channel_notification {
        return Some((conversation_title.to_string(), body));
    }

    let sender = message
        .user
        .as_deref()
        .and_then(|user_id| user_names.get(user_id).cloned())
        .or_else(|| message.user.is_none().then(|| message.author_label()))?;
    Some((conversation_title.to_string(), format!("{sender}: {body}")))
}

fn message_notification_conversation(
    conversation: Option<&SlackConversation>,
    user_names: &HashMap<String, String>,
    user_full_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
) -> Option<(String, bool)> {
    let Some(conversation) = conversation else {
        return Some((gettext("Slack"), false));
    };
    let resolved_user = |user_id: &str| {
        user_full_names
            .get(user_id)
            .or_else(|| user_names.get(user_id))
            .is_some_and(|name| !name.trim().is_empty())
    };

    if conversation.is_im.unwrap_or(false) {
        let Some(user_id) = conversation.user.as_deref() else {
            return Some((gettext("Direct message"), false));
        };
        if !resolved_user(user_id) {
            return None;
        }
        return Some((
            conversation.navigation_name_with_users(user_names, user_full_names, current_user_id),
            false,
        ));
    }

    if conversation.is_mpim.unwrap_or(false) {
        let participants = conversation
            .group_direct_message_user_ids()
            .into_iter()
            .filter(|user_id| Some(user_id.as_str()) != current_user_id)
            .collect::<Vec<_>>();
        if participants.iter().any(|user_id| !resolved_user(user_id)) {
            return None;
        }
        let title = if participants.is_empty() {
            gettext("Group direct message")
        } else {
            conversation.navigation_name_with_users(user_names, user_full_names, current_user_id)
        };
        return Some((title, false));
    }

    Some((
        conversation.navigation_name_with_users(user_names, user_full_names, current_user_id),
        conversation.is_channel.unwrap_or(false)
            || conversation.is_group.unwrap_or(false)
            || conversation.is_private.unwrap_or(false),
    ))
}

fn local_reaction_update(
    channel_id: &str,
    ts: &str,
    name: &str,
    added: bool,
    current_user_id: Option<&str>,
) -> Option<ReactionUpdate> {
    Some(ReactionUpdate {
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        name: name.to_string(),
        user_id: current_user_id?.to_string(),
        added,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationTarget {
    workspace_id: String,
    channel_id: String,
    thread_ts: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingMessageNotification {
    channel_id: String,
    message: SlackMessage,
    decision: AttentionDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationTargetResolution {
    Wait,
    Open,
    RejectWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationTargetAction {
    SelectConversation(String),
    OpenDirectMessage(String),
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

fn conversation_target_action(
    channel_or_user_id: &str,
    conversations: &[SlackConversation],
) -> ConversationTargetAction {
    if channel_or_user_id.starts_with('U') || channel_or_user_id.starts_with('W') {
        if let Some(channel_id) = conversations
            .iter()
            .find(|conversation| {
                conversation.is_im.unwrap_or(false)
                    && conversation.user.as_deref() == Some(channel_or_user_id)
            })
            .map(|conversation| conversation.id.clone())
        {
            return ConversationTargetAction::SelectConversation(channel_id);
        }
        return ConversationTargetAction::OpenDirectMessage(channel_or_user_id.to_string());
    }
    ConversationTargetAction::SelectConversation(channel_or_user_id.to_string())
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

fn draft_persist_required(drafts_changed: bool, persist_pending: bool) -> bool {
    drafts_changed || persist_pending
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
    ConversationStar,
    UserStatus,
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
        (RuntimeOperation::Files, RuntimeTarget::Workspace | RuntimeTarget::File(_)) => {
            RuntimeFailureRecovery::Files
        }
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
        (RuntimeOperation::MessagePermalink, RuntimeTarget::ExactMessage { .. }) => {
            RuntimeFailureRecovery::NonDisruptive
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
        (RuntimeOperation::ConversationStar, RuntimeTarget::Channel(_)) => {
            RuntimeFailureRecovery::ConversationStar
        }
        (RuntimeOperation::UserStatus, RuntimeTarget::Workspace) => {
            RuntimeFailureRecovery::UserStatus
        }
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

fn current_user_status_error_message(failure: &RuntimeFailure) -> String {
    if failure.category == RuntimeFailureCategory::Validation
        && failure.message.contains("for this conversation")
    {
        gettext(
            "Slack did not allow Conduit to change your status. Reconnect the workspace with profile access and try again.",
        )
    } else if failure.category == RuntimeFailureCategory::Internal {
        gettext("Slack could not change your status. Try again.")
    } else {
        failure.message.clone()
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

fn status_expiration_for_choice(
    choice: StatusExpirationChoice,
    now: i64,
    end_today: i64,
    end_week: i64,
) -> i64 {
    match choice {
        StatusExpirationChoice::Never => 0,
        StatusExpirationChoice::Minutes30 => now.saturating_add(30 * 60),
        StatusExpirationChoice::Hour1 => now.saturating_add(60 * 60),
        StatusExpirationChoice::Hours4 => now.saturating_add(4 * 60 * 60),
        StatusExpirationChoice::Today => end_today,
        StatusExpirationChoice::ThisWeek => end_week,
        StatusExpirationChoice::Existing(expiration) => expiration,
    }
}

fn status_from_dialog_input(
    text: &str,
    emoji: &str,
    expiration_choice: StatusExpirationChoice,
    now: i64,
    end_today: i64,
    end_week: i64,
) -> SlackUserStatus {
    SlackUserStatus {
        text: text.trim().chars().take(100).collect(),
        emoji: emoji.trim().trim_matches(':').to_string(),
        expiration: status_expiration_for_choice(expiration_choice, now, end_today, end_week),
    }
}

fn status_expiration_boundaries(now: i64) -> (i64, i64) {
    let fallback = (
        now.saturating_add(24 * 60 * 60),
        now.saturating_add(7 * 24 * 60 * 60),
    );
    let Ok(local) = glib::DateTime::now_local() else {
        return fallback;
    };
    let Ok(end_today) = glib::DateTime::from_local(
        local.year(),
        local.month(),
        local.day_of_month(),
        23,
        59,
        59.0,
    ) else {
        return fallback;
    };
    let Ok(end_week_date) = local.add_days(7_i32.saturating_sub(local.day_of_week())) else {
        return (end_today.to_unix(), fallback.1);
    };
    let Ok(end_week) = glib::DateTime::from_local(
        end_week_date.year(),
        end_week_date.month(),
        end_week_date.day_of_month(),
        23,
        59,
        59.0,
    ) else {
        return (end_today.to_unix(), fallback.1);
    };
    (end_today.to_unix(), end_week.to_unix())
}

fn user_status_presentation(
    status: &SlackUserStatus,
    custom_emojis: &HashMap<String, String>,
    now: i64,
) -> Option<UserStatusPresentation> {
    if !status.active_at(now) {
        return None;
    }
    let text = status.text.trim();
    let emoji = (!status.emoji_name().is_empty()).then(|| {
        EmojiCatalog::new(custom_emojis)
            .resolve(status.emoji_name())
            .and_then(|value| match value {
                EmojiValue::Unicode(glyph) => Some(glyph.to_string()),
                EmojiValue::CustomImage(_) => None,
            })
            .unwrap_or_else(|| "●".to_string())
    });
    let subtitle = match (emoji.as_deref(), text.is_empty()) {
        (Some(emoji), false) => format!("{emoji} {text}"),
        (Some(emoji), true) => emoji.to_string(),
        (None, false) => text.to_string(),
        (None, true) => return None,
    };
    Some(UserStatusPresentation {
        subtitle,
        accessible_text: status.accessible_text(),
    })
}

fn apply_user_status_profile_update(
    statuses: &mut HashMap<String, SlackUserStatus>,
    user_id: &str,
    profile: &SlackUserProfile,
) -> bool {
    if !profile.contains_status_fields() {
        return false;
    }
    match profile.status() {
        Some(status) if statuses.get(user_id) == Some(&status) => false,
        Some(status) => {
            statuses.insert(user_id.to_string(), status);
            true
        }
        None => statuses.remove(user_id).is_some(),
    }
}

fn apply_user_status_snapshot(
    current: &mut HashMap<String, SlackUserStatus>,
    statuses: HashMap<String, SlackUserStatus>,
    replace_existing: bool,
    preserve_user_ids: &HashSet<String>,
) -> Vec<String> {
    let previous = current.clone();
    if replace_existing {
        let mut next = statuses;
        for user_id in preserve_user_ids {
            if let Some(status) = previous.get(user_id) {
                next.insert(user_id.clone(), status.clone());
            } else {
                next.remove(user_id);
            }
        }
        *current = next;
    } else {
        for (user_id, status) in statuses {
            if !preserve_user_ids.contains(&user_id) {
                current.entry(user_id).or_insert(status);
            }
        }
    }

    previous
        .keys()
        .chain(current.keys())
        .filter(|user_id| previous.get(*user_id) != current.get(*user_id))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn current_user_header_title(
    current_user_id: Option<&str>,
    user_names: &HashMap<String, String>,
    workspace_name: Option<&str>,
) -> String {
    current_user_id
        .and_then(|user_id| user_names.get(user_id))
        .map(String::as_str)
        .or(workspace_name)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| gettext("Workspace"))
}

impl StatusEmojiPickerModel {
    fn new(custom_emojis: &HashMap<String, String>, selected_emoji: &str) -> Self {
        // Slack status emoji are submitted as team-enabled shortcodes. Keep the
        // picker to catalog entries that have a valid shortcode name.
        let catalog_entries = EmojiCatalog::new(custom_emojis).entries();
        let workspace_names = catalog_entries
            .iter()
            .filter(|entry| entry.category == "Workspace")
            .map(|entry| entry.name.clone())
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        let mut entries = catalog_entries
            .into_iter()
            .filter(|entry| entry.category == "Workspace" || !workspace_names.contains(&entry.name))
            .filter(|entry| seen.insert(entry.name.clone()))
            .collect::<Vec<_>>();

        let selected_emoji = selected_emoji.trim().trim_matches(':');
        if !selected_emoji.is_empty() && seen.insert(selected_emoji.to_string()) {
            entries.push(EmojiEntry {
                name: selected_emoji.to_string(),
                label: selected_emoji.replace(['_', '-'], " "),
                category: "Current status",
                value: EmojiValue::CustomImage(String::new()),
            });
        }

        Self {
            emojis: EmojiPickerModel::new(entries),
        }
    }

    fn choice_count(&self) -> usize {
        self.emojis.entries().len() + 1
    }

    fn contains(&self, name: &str) -> bool {
        name.is_empty() || self.emojis.entries().iter().any(|entry| entry.name == name)
    }

    fn selected_label(&self, name: &str) -> String {
        if name.is_empty() {
            return gettext("No emoji");
        }
        self.emojis
            .entries()
            .iter()
            .find(|entry| entry.name == name)
            .map(status_emoji_entry_label)
            .unwrap_or_else(|| format!(":{name}:"))
    }

    fn search(&self, query: &str) -> Vec<StatusEmojiChoice> {
        let normalized_query = query.trim().to_lowercase();
        let mut choices = Vec::new();
        if normalized_query.is_empty()
            || gettext("No emoji")
                .to_lowercase()
                .contains(&normalized_query)
        {
            choices.push(StatusEmojiChoice {
                name: String::new(),
                label: gettext("No emoji"),
            });
        }
        choices.extend(
            self.emojis
                .search(query)
                .into_iter()
                .map(|entry| StatusEmojiChoice {
                    name: entry.name.clone(),
                    label: status_emoji_entry_label(&entry),
                }),
        );
        choices
    }
}

fn status_emoji_entry_label(entry: &EmojiEntry) -> String {
    match entry.value {
        EmojiValue::Unicode(glyph) => format!("{glyph} :{}: - {}", entry.name, entry.label),
        EmojiValue::CustomImage(_) => format!(":{}: - {}", entry.name, entry.label),
    }
}

fn populate_status_emoji_picker_results(
    source: &StatusEmojiPickerModel,
    query: &str,
    visible_model: &gtk::StringList,
    visible_choices: &RefCell<Vec<StatusEmojiChoice>>,
) {
    let choices = source.search(query);
    let labels = choices
        .iter()
        .map(|choice| choice.label.as_str())
        .collect::<Vec<_>>();
    visible_model.splice(0, visible_model.n_items(), &labels);
    visible_choices.replace(choices);
}

fn sync_status_emoji_picker_selection(
    visible_choices: &RefCell<Vec<StatusEmojiChoice>>,
    selected_name: &RefCell<String>,
    selection: &gtk::SingleSelection,
) {
    let selected_name = selected_name.borrow();
    let position = visible_choices
        .borrow()
        .iter()
        .position(|choice| choice.name == selected_name.as_str())
        .map(|position| position as u32)
        .unwrap_or(gtk::INVALID_LIST_POSITION);
    selection.set_selected(position);
}

impl StatusEmojiPicker {
    fn new(
        custom_emojis: &HashMap<String, String>,
        selected_emoji: &str,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        let selected_name = Rc::new(RefCell::new(
            selected_emoji.trim().trim_matches(':').to_string(),
        ));
        let source = Rc::new(RefCell::new(StatusEmojiPickerModel::new(
            custom_emojis,
            selected_emoji,
        )));
        let visible_model = gtk::StringList::new(&[]);
        let visible_choices = Rc::new(RefCell::new(Vec::new()));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let label = gtk::Label::new(None);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_hexpand(true);
            label.set_margin_top(6);
            label.set_margin_bottom(6);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_xalign(0.0);
            item.set_child(Some(&label));
        });
        factory.connect_bind(|_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(value) = item.item().and_downcast::<gtk::StringObject>() else {
                return;
            };
            label.set_label(&value.string());
            label.update_property(&[gtk::accessible::Property::Label(value.string().as_str())]);
        });

        let selection = gtk::SingleSelection::new(Some(visible_model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        selection.set_selected(gtk::INVALID_LIST_POSITION);
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.set_single_click_activate(true);
        list.add_css_class("navigation-sidebar");
        list.update_property(&[gtk::accessible::Property::Label(&gettext(
            "Status emoji choices",
        ))]);

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some(&gettext("Search emoji")));
        search.update_property(&[gtk::accessible::Property::Label(&gettext("Search emoji"))]);
        search.set_key_capture_widget(Some(&list));

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_min_content_width(320);
        scroller.set_min_content_height(280);
        scroller.set_max_content_height(360);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&list));

        let picker_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        picker_content.set_margin_top(8);
        picker_content.set_margin_bottom(8);
        picker_content.set_margin_start(8);
        picker_content.set_margin_end(8);
        picker_content.append(&search);
        picker_content.append(&scroller);

        let popover = gtk::Popover::new();
        popover.set_autohide(true);
        popover.set_child(Some(&picker_content));

        let button = gtk::MenuButton::new();
        button.set_icon_name("pan-down-symbolic");
        button.set_popover(Some(&popover));
        button.set_tooltip_text(Some(&gettext("Choose a status emoji")));
        button.set_valign(gtk::Align::Center);
        button.update_property(&[gtk::accessible::Property::Label(&gettext(
            "Choose a status emoji",
        ))]);

        let selected_label = source.borrow().selected_label(&selected_name.borrow());
        let row = adw::ActionRow::builder()
            .title(gettext("Emoji"))
            .subtitle(selected_label)
            .build();
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));

        populate_status_emoji_picker_results(
            &source.borrow(),
            "",
            &visible_model,
            &visible_choices,
        );
        sync_status_emoji_picker_selection(&visible_choices, &selected_name, &selection);

        {
            let source = source.clone();
            let visible_model = visible_model.clone();
            let visible_choices = visible_choices.clone();
            let selected_name = selected_name.clone();
            let selection = selection.clone();
            search.connect_search_changed(move |search| {
                populate_status_emoji_picker_results(
                    &source.borrow(),
                    search.text().as_str(),
                    &visible_model,
                    &visible_choices,
                );
                sync_status_emoji_picker_selection(&visible_choices, &selected_name, &selection);
            });
        }

        let on_selected: Rc<dyn Fn(&str)> = Rc::new(on_selected);
        {
            let visible_choices = visible_choices.clone();
            let selected_name = selected_name.clone();
            let weak_row = row.downgrade();
            let weak_popover = popover.downgrade();
            let weak_search = search.downgrade();
            let on_selected = on_selected.clone();
            list.connect_activate(move |_, position| {
                let Some(choice) = visible_choices.borrow().get(position as usize).cloned() else {
                    return;
                };
                selected_name.replace(choice.name.clone());
                if let Some(row) = weak_row.upgrade() {
                    row.set_subtitle(&choice.label);
                }
                on_selected(&choice.name);
                if let Some(search) = weak_search.upgrade() {
                    search.set_text("");
                }
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
            });
        }

        {
            let visible_choices = visible_choices.clone();
            let selected_name = selected_name.clone();
            let weak_row = row.downgrade();
            let weak_popover = popover.downgrade();
            let on_selected = on_selected.clone();
            search.connect_activate(move |search| {
                if search.text().trim().is_empty() {
                    return;
                }
                let Some(choice) = visible_choices.borrow().first().cloned() else {
                    return;
                };
                selected_name.replace(choice.name.clone());
                if let Some(row) = weak_row.upgrade() {
                    row.set_subtitle(&choice.label);
                }
                on_selected(&choice.name);
                search.set_text("");
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
            });
        }

        {
            let weak_list = list.downgrade();
            let selection = selection.clone();
            let visible_model = visible_model.clone();
            let controller = gtk::EventControllerKey::new();
            controller.connect_key_pressed(move |_, key, _, _| {
                if key != gtk::gdk::Key::Down {
                    return glib::Propagation::Proceed;
                }
                if visible_model.n_items() > 0 {
                    selection.set_selected(0);
                    if let Some(list) = weak_list.upgrade() {
                        list.grab_focus();
                    }
                }
                glib::Propagation::Stop
            });
            search.add_controller(controller);
        }

        {
            let weak_search = search.downgrade();
            let weak_list = list.downgrade();
            let selection = selection.clone();
            popover.connect_visible_notify(move |popover| {
                if popover.is_visible() {
                    if let Some(search) = weak_search.upgrade() {
                        search.grab_focus();
                    }
                    let selected = selection.selected();
                    if selected != gtk::INVALID_LIST_POSITION {
                        if let Some(list) = weak_list.upgrade() {
                            list.scroll_to(selected, gtk::ListScrollFlags::NONE, None);
                        }
                    }
                }
            });
        }

        {
            let weak_popover = popover.downgrade();
            search.connect_stop_search(move |_| {
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
            });
        }

        {
            let weak_search = search.downgrade();
            popover.connect_closed(move |_| {
                if let Some(search) = weak_search.upgrade() {
                    search.set_text("");
                    search.emit_by_name::<()>("search-changed", &[]);
                }
            });
        }

        if let Some(query) = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_QUERY") {
            let query = query.to_string_lossy();
            search.set_text(&query);
            search.emit_by_name::<()>("search-changed", &[]);
        }

        Self {
            row,
            popover,
            search,
            visible_model,
            visible_choices,
            selection,
            source,
            selected_name,
        }
    }

    fn selected_name(&self) -> String {
        self.selected_name.borrow().clone()
    }

    fn selected_name_state(&self) -> Rc<RefCell<String>> {
        self.selected_name.clone()
    }

    fn source_choice_count(&self) -> usize {
        self.source.borrow().choice_count()
    }

    fn visible_choice_count(&self) -> u32 {
        self.visible_model.n_items()
    }

    fn first_visible_name(&self) -> Option<String> {
        self.visible_choices
            .borrow()
            .first()
            .map(|choice| choice.name.clone())
    }

    fn selected_visible_name(&self) -> Option<String> {
        let selected = self.selection.selected();
        if selected == gtk::INVALID_LIST_POSITION {
            return None;
        }
        self.visible_choices
            .borrow()
            .get(selected as usize)
            .map(|choice| choice.name.clone())
    }

    fn contains(&self, name: &str) -> bool {
        self.source.borrow().contains(name)
    }

    fn refresh_catalog(&self, custom_emojis: &HashMap<String, String>) {
        let selected_name = self.selected_name();
        self.source
            .replace(StatusEmojiPickerModel::new(custom_emojis, &selected_name));
        self.row
            .set_subtitle(&self.source.borrow().selected_label(&selected_name));
        populate_status_emoji_picker_results(
            &self.source.borrow(),
            self.search.text().as_str(),
            &self.visible_model,
            &self.visible_choices,
        );
        sync_status_emoji_picker_selection(
            &self.visible_choices,
            &self.selected_name,
            &self.selection,
        );
    }
}

fn status_expiration_options(
    existing_expiration: i64,
    now: i64,
) -> (Vec<String>, Vec<StatusExpirationChoice>, u32) {
    let mut labels = vec![
        gettext("Don't clear"),
        gettext("30 minutes"),
        gettext("1 hour"),
        gettext("4 hours"),
        gettext("End of today"),
        gettext("End of this week"),
    ];
    let mut choices = vec![
        StatusExpirationChoice::Never,
        StatusExpirationChoice::Minutes30,
        StatusExpirationChoice::Hour1,
        StatusExpirationChoice::Hours4,
        StatusExpirationChoice::Today,
        StatusExpirationChoice::ThisWeek,
    ];
    let selected = if existing_expiration > now {
        let formatted = glib::DateTime::from_unix_local(existing_expiration)
            .ok()
            .and_then(|date_time| date_time.format("%a %H:%M").ok())
            .map(|date_time| date_time.to_string())
            .unwrap_or_else(|| existing_expiration.to_string());
        labels.push(
            gettext("Keep current clear time ({time})").replace("{time}", formatted.as_str()),
        );
        choices.push(StatusExpirationChoice::Existing(existing_expiration));
        choices.len() - 1
    } else {
        0
    };
    (labels, choices, selected as u32)
}

fn update_status_dialog_save_response(
    dialog: &adw::AlertDialog,
    status_entry: &adw::EntryRow,
    selected_emoji: &str,
) {
    dialog.set_response_enabled(
        "save",
        !status_entry.text().trim().is_empty() || !selected_emoji.is_empty(),
    );
}

fn status_dialog_clear_available(status: &SlackUserStatus, now: i64, clearing_retry: bool) -> bool {
    clearing_retry || status.active_at(now)
}

fn enforce_status_text_limit(status_entry: &adw::EntryRow) {
    let text = status_entry.text();
    if text.chars().count() <= 100 {
        return;
    }
    let limited = text.chars().take(100).collect::<String>();
    status_entry.set_text(&limited);
    status_entry.set_position(-1);
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
    workspace_interactive: bool,
}

fn workspace_lifecycle_presentation(
    lifecycle: WorkspaceLifecycle,
    workspace_available: bool,
    initial_sync_complete: bool,
) -> WorkspaceLifecyclePresentation {
    use WorkspaceLifecycle as Lifecycle;
    use WorkspaceLifecycleSurface as Surface;

    match lifecycle {
        Lifecycle::Disconnected => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Choose a workspace to continue",
            workspace_interactive: false,
        },
        Lifecycle::Connecting => WorkspaceLifecyclePresentation {
            surface: Surface::Loading,
            status: "Connecting to Slack…",
            workspace_interactive: false,
        },
        Lifecycle::Syncing => WorkspaceLifecyclePresentation {
            surface: if workspace_available && initial_sync_complete {
                Surface::Workspace
            } else {
                Surface::Loading
            },
            status: "Syncing workspace…",
            workspace_interactive: initial_sync_complete,
        },
        Lifecycle::Ready => WorkspaceLifecyclePresentation {
            surface: Surface::Workspace,
            status: "",
            workspace_interactive: true,
        },
        Lifecycle::Degraded => WorkspaceLifecyclePresentation {
            surface: if workspace_available && initial_sync_complete {
                Surface::Workspace
            } else if workspace_available {
                Surface::Loading
            } else {
                Surface::Connect
            },
            status: "Connection interrupted. Retrying…",
            workspace_interactive: initial_sync_complete,
        },
        Lifecycle::AuthenticationRequired => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Slack authentication failed. Sign in again.",
            workspace_interactive: false,
        },
        Lifecycle::StartupFailed => WorkspaceLifecyclePresentation {
            surface: Surface::Connect,
            status: "Conduit could not start.",
            workspace_interactive: false,
        },
    }
}

fn initial_sync_completion(completed_before: bool, lifecycle: WorkspaceLifecycle) -> bool {
    match lifecycle {
        WorkspaceLifecycle::Ready => true,
        WorkspaceLifecycle::Syncing | WorkspaceLifecycle::Degraded => completed_before,
        WorkspaceLifecycle::Disconnected
        | WorkspaceLifecycle::Connecting
        | WorkspaceLifecycle::AuthenticationRequired
        | WorkspaceLifecycle::StartupFailed => false,
    }
}

#[derive(Debug, Default)]
pub(super) struct MembershipReactivationState {
    has_activated: bool,
    was_active: bool,
}

impl MembershipReactivationState {
    fn observe(&mut self, is_active: bool) -> bool {
        let reactivated = self.has_activated && !self.was_active && is_active;
        if is_active {
            self.has_activated = true;
        }
        self.was_active = is_active;
        reactivated
    }
}

fn membership_reactivation_should_refresh(
    reactivated: bool,
    workspace_connected: bool,
    realtime_status: RealtimeStatus,
) -> bool {
    reactivated && workspace_connected && realtime_status.transport.is_none()
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
) -> bool {
    conversations.iter().any(|conversation| {
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

fn emoji_picker_query_from_json(json: &str) -> Option<EmojiPickerQuery> {
    serde_json::from_str(json).ok()
}

fn emoji_picker_query_from_value(
    value: &webkit6::javascriptcore::Value,
) -> Option<EmojiPickerQuery> {
    let json = value.to_json(0)?;
    emoji_picker_query_from_json(json.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimelineLifecycleAction {
    Positioned(u64),
    Interacted(u64),
}

fn timeline_lifecycle_action(url: &url::Url) -> Option<TimelineLifecycleAction> {
    let generation = query_param(url, "generation")?.parse::<u64>().ok()?;
    if generation == 0 {
        return None;
    }
    match url.host_str()? {
        "timeline-positioned" => Some(TimelineLifecycleAction::Positioned(generation)),
        "timeline-interacted" => Some(TimelineLifecycleAction::Interacted(generation)),
        _ => None,
    }
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

fn attachment_image_asset_request(
    attachment: &crate::models::SlackAttachment,
) -> Option<(String, String)> {
    let url = attachment
        .image_url
        .as_deref()
        .or(attachment.thumb_url.as_deref())?;
    Some((url.to_string(), url.to_string()))
}

fn message_image_asset_requests<'a>(
    messages: impl IntoIterator<Item = &'a SlackMessage>,
    avatar_urls: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut requests = Vec::new();
    for message in messages {
        requests.extend(
            message
                .files
                .as_ref()
                .into_iter()
                .flatten()
                .filter_map(image_asset_request),
        );
        requests.extend(
            message
                .attachments
                .as_ref()
                .into_iter()
                .flatten()
                .filter_map(attachment_image_asset_request),
        );
        if let Some(url) = message
            .user
            .as_ref()
            .and_then(|user_id| avatar_urls.get(user_id))
            .map(String::as_str)
            .or_else(|| message.avatar_url())
        {
            requests.push((url.to_string(), url.to_string()));
        }
    }
    requests.sort_by(|left, right| left.0.cmp(&right.0));
    requests.dedup_by(|left, right| left.0 == right.0);
    requests
}

fn messages_use_image_asset(messages: &[SlackMessage], key: &str) -> bool {
    messages.iter().any(|message| {
        message.avatar_url() == Some(key)
            || message
                .files
                .as_ref()
                .into_iter()
                .flatten()
                .filter_map(image_asset_request)
                .any(|(candidate, _)| candidate == key)
            || message
                .attachments
                .as_ref()
                .into_iter()
                .flatten()
                .filter_map(attachment_image_asset_request)
                .any(|(candidate, _)| candidate == key)
    })
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

fn timeline_patch_needed(
    state_changed: bool,
    arrival: Option<TimelineMessageArrival>,
    surface_contains_message: bool,
) -> bool {
    state_changed || (arrival.is_some() && surface_contains_message)
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
    crate::slack::constructed_message_permalink(workspace_url, channel_id, ts)
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

fn actively_reading_channel(
    window_active: bool,
    selected_channel: Option<&str>,
    channel_id: &str,
) -> bool {
    window_active && selected_channel == Some(channel_id)
}

fn attention_notification_should_deliver(
    window_active: bool,
    selected_channel: Option<&str>,
    channel_id: &str,
    muted: bool,
) -> bool {
    !muted && !actively_reading_channel(window_active, selected_channel, channel_id)
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
    resolve_first_unread_message_ts(messages, last_read, unread_count)
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

fn configure_message_web_view_settings(settings: &webkit6::Settings) {
    settings.set_allow_file_access_from_file_urls(false);
    settings.set_allow_universal_access_from_file_urls(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(true);
    settings.set_enable_javascript(true);
    settings.set_enable_media(true);
    settings.set_enable_webgl(false);
    settings.set_enable_webaudio(false);
    settings.set_zoom_text_only(true);
}

fn message_web_view_context_menu_action_is_unsupported(action: webkit6::ContextMenuAction) -> bool {
    matches!(
        action,
        webkit6::ContextMenuAction::GoBack
            | webkit6::ContextMenuAction::GoForward
            | webkit6::ContextMenuAction::Stop
            | webkit6::ContextMenuAction::Reload
    )
}

fn prune_message_web_view_context_menu(context_menu: &webkit6::ContextMenu) {
    for item in context_menu.items() {
        if message_web_view_context_menu_action_is_unsupported(item.stock_action()) {
            context_menu.remove(&item);
        }
    }
}

fn message_text_zoom(font_name: Option<&str>) -> f64 {
    let Some(font_name) = font_name else {
        return 1.0;
    };
    let description = gtk::pango::FontDescription::from_string(font_name);
    let size = f64::from(description.size()) / f64::from(gtk::pango::SCALE);
    if !size.is_finite() || size <= 0.0 {
        return 1.0;
    }

    let css_pixels = if description.is_size_absolute() {
        size
    } else {
        size * 96.0 / 72.0
    };
    css_pixels / message_html::MESSAGE_BASE_FONT_SIZE_CSS_PX
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

    pub(crate) fn realtime_status(&self) -> RealtimeStatus {
        self.imp().realtime_status.get()
    }

    pub(crate) fn connect_realtime_status_changed(
        &self,
        callback: impl Fn(&Self) + 'static,
    ) -> glib::SignalHandlerId {
        self.connect_local("realtime-status-changed", false, move |values| {
            let window = values[0]
                .get::<Self>()
                .expect("realtime status signal emitted by ConduitWindow");
            callback(&window);
            None
        })
    }

    fn set_realtime_status(&self, status: RealtimeStatus) {
        if self.imp().realtime_status.replace(status) == status {
            return;
        }
        self.render_workspace_lifecycle();
        self.emit_by_name::<()>("realtime-status-changed", &[]);
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
        imp.huddle_revealer
            .update_property(&[gtk::accessible::Property::Label("Slack huddle controls")]);

        for (button, label) in [
            (&imp.huddle_primary_button, "Huddle action"),
            (&imp.huddle_external_button, "Open huddle in Slack"),
            (&imp.huddle_mute_button, "Mute microphone"),
            (&imp.huddle_camera_button, "Turn camera on"),
            (&imp.huddle_share_button, "Share screen"),
            (&imp.huddle_leave_button, "Leave huddle"),
            (&imp.huddle_dismiss_button, "Dismiss huddle"),
        ] {
            button.update_property(&[gtk::accessible::Property::Label(label)]);
        }

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
        let font_settings = gtk::Settings::default();
        let text_zoom = message_text_zoom(
            font_settings
                .as_ref()
                .and_then(gtk::Settings::gtk_font_name)
                .as_deref(),
        );

        let message_view = self.create_message_web_view(&network_session, text_zoom);
        let viewer = self.create_media_viewer(&message_view);
        self.imp().message_view_box.append(&viewer.surface_stack);
        *self.imp().message_view.borrow_mut() = Some(message_view.clone());
        *self.imp().media_viewer.borrow_mut() = Some(viewer);
        self.setup_media_viewer_callbacks();

        let thread_view = self.create_message_web_view(&network_session, text_zoom);
        let weak_message_view = message_view.downgrade();
        let weak_thread_view = thread_view.downgrade();
        let thread_pane = ThreadPane::new(
            &self.imp().thread_split.get(),
            &self.imp().thread_title.get(),
            &self.imp().thread_view_box.get(),
            thread_view,
        );
        *self.imp().thread_pane_controller.borrow_mut() = Some(thread_pane);

        if let Some(font_settings) = font_settings {
            let handler = font_settings.connect_gtk_font_name_notify(move |settings| {
                let text_zoom = message_text_zoom(settings.gtk_font_name().as_deref());
                for weak_view in [&weak_message_view, &weak_thread_view] {
                    if let Some(view) = weak_view.upgrade() {
                        view.set_zoom_level(text_zoom);
                    }
                }
            });
            *self.imp().message_font_settings_handler.borrow_mut() = Some((font_settings, handler));
        }

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
        text_zoom: f64,
    ) -> webkit6::WebView {
        let settings = webkit6::Settings::new();
        configure_message_web_view_settings(&settings);
        let user_content_manager = webkit6::UserContentManager::new();
        let picker_handler_registered = user_content_manager
            .register_script_message_handler(EMOJI_PICKER_MESSAGE_HANDLER, None);

        let web_view = webkit6::WebView::builder()
            .network_session(network_session)
            .settings(&settings)
            .user_content_manager(&user_content_manager)
            .build();
        web_view.set_hexpand(true);
        web_view.set_vexpand(true);
        web_view.set_zoom_level(text_zoom);

        if picker_handler_registered {
            let weak_window = self.downgrade();
            let weak_web_view = web_view.downgrade();
            let generation_gate = Rc::new(RefCell::new(EmojiPickerGenerationGate::default()));
            user_content_manager.connect_script_message_received(
                Some(EMOJI_PICKER_MESSAGE_HANDLER),
                move |_, value| {
                    let Some(window) = weak_window.upgrade() else {
                        return;
                    };
                    let Some(web_view) = weak_web_view.upgrade() else {
                        return;
                    };
                    let mut generation_gate = generation_gate.borrow_mut();
                    window.handle_emoji_picker_query(&web_view, &mut generation_gate, value);
                },
            );
        }

        web_view.connect_context_menu(|_, context_menu, _| {
            prune_message_web_view_context_menu(context_menu);
            false
        });

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

    fn reaction_emoji_picker_model(&self) -> Arc<EmojiPickerModel> {
        if let Some(model) = self.imp().reaction_emoji_picker_model.borrow().as_ref() {
            return model.clone();
        }
        let model = Arc::new(EmojiPickerModel::new(
            EmojiCatalog::new(&self.imp().custom_emojis.borrow()).entries(),
        ));
        *self.imp().reaction_emoji_picker_model.borrow_mut() = Some(model.clone());
        model
    }

    fn handle_emoji_picker_query(
        &self,
        web_view: &webkit6::WebView,
        generation_gate: &mut EmojiPickerGenerationGate,
        value: &webkit6::javascriptcore::Value,
    ) {
        let Some(query) = emoji_picker_query_from_value(value) else {
            return;
        };
        let Some(result) = self.reaction_emoji_picker_model().query(&query) else {
            return;
        };
        if !generation_gate.accept(query.generation) {
            return;
        }
        let Ok(payload) = serde_json::to_string(&result) else {
            return;
        };
        let arguments = glib::VariantDict::new(None);
        arguments.insert("payload", payload.as_str());
        let arguments = arguments.end();
        web_view.call_async_javascript_function(
            APPLY_EMOJI_PICKER_RESULT_SCRIPT,
            Some(&arguments),
            None,
            None,
            None::<&gio::Cancellable>,
            |_| {},
        );
    }

    fn setup_reaction_picker_escape_fallback(&self) {
        // WebKitGTK does not consistently forward Escape to an open HTML
        // dialog. Capture it at the application window and dispatch the
        // dialog's existing cancellation path into both timeline WebViews.
        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_window = self.downgrade();
        controller.connect_key_pressed(move |_, key, _, _| {
            if key != gtk::gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            let Some(window) = weak_window.upgrade() else {
                return glib::Propagation::Proceed;
            };

            let main_view = window.imp().message_view.borrow().clone();
            let thread_view = window.thread_pane().web_view();
            for web_view in main_view.into_iter().chain([thread_view]) {
                web_view.evaluate_javascript(
                    CANCEL_REACTION_PICKER_SCRIPT,
                    None,
                    None,
                    None::<&gio::Cancellable>,
                    |_| {},
                );
            }
            glib::Propagation::Proceed
        });
        self.add_controller(controller);
    }

    fn setup_callbacks(&self) {
        let imp = self.imp();

        clear_stale_upload_staging();
        self.setup_window_actions();
        imp.membership_reactivation
            .borrow_mut()
            .observe(self.is_active());
        self.connect_is_active_notify(move |window| {
            let reactivated = window
                .imp()
                .membership_reactivation
                .borrow_mut()
                .observe(window.is_active());
            if membership_reactivation_should_refresh(
                reactivated,
                window.imp().workspace_id.borrow().is_some(),
                window.realtime_status(),
            ) {
                window.send_command(RuntimeCommand::RefreshConversationsIfStale);
            }
        });
        self.connect_close_request(|window| {
            window.flush_persistent_state();
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
        self.connect_widget(&imp.huddle_primary_button.get(), |window| {
            window.activate_huddle_primary_action()
        });
        self.connect_widget(&imp.huddle_external_button.get(), |window| {
            window.open_active_huddle_externally()
        });
        self.connect_widget(&imp.huddle_mute_button.get(), |window| {
            window.toggle_huddle_mute()
        });
        self.connect_widget(&imp.huddle_camera_button.get(), |window| {
            window.toggle_huddle_camera()
        });
        self.connect_widget(&imp.huddle_share_button.get(), |window| {
            window.toggle_huddle_screen_share()
        });
        self.connect_widget(&imp.huddle_leave_button.get(), |window| {
            window.send_command(RuntimeCommand::Huddle(HuddleCommand::Leave))
        });
        self.connect_widget(&imp.huddle_dismiss_button.get(), |window| {
            window.dismiss_huddle()
        });

        for target in COMPOSER_TARGETS {
            self.setup_composer_completion(target);
        }

        let weak_window = self.downgrade();
        imp.browser_session_check.connect_toggled(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.update_auth_mode_ui();
            }
        });

        let weak_window = self.downgrade();
        imp.browser_session_howto_link
            .connect_activate_link(move |button| {
                if let Some(window) = weak_window.upgrade() {
                    window.open_external_link(button.uri().as_str());
                }
                glib::Propagation::Stop
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
        imp.sidebar_all_filter_button.connect_toggled(move |_| {
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
        self.restore_window_state(&settings);
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
        let weak_window = self.downgrade();
        settings.connect_changed(None, move |_, key| {
            if attention_settings::is_attention_setting(key) {
                if let Some(window) = weak_window.upgrade() {
                    window.sync_attention_preferences();
                }
            }
        });
        *self.imp().settings.borrow_mut() = Some(settings);
        self.sync_attention_preferences();
    }

    fn restore_window_state(&self, settings: &gio::Settings) {
        self.set_default_size(
            settings.int(config::WINDOW_WIDTH_KEY),
            settings.int(config::WINDOW_HEIGHT_KEY),
        );
        if settings.boolean(config::WINDOW_MAXIMIZED_KEY) {
            self.maximize();
        }
    }

    fn save_window_state(&self) {
        let Some(settings) = self.imp().settings.borrow().as_ref().cloned() else {
            return;
        };
        let (width, height) = self.default_size();
        for result in [
            settings.set_int(config::WINDOW_WIDTH_KEY, width),
            settings.set_int(config::WINDOW_HEIGHT_KEY, height),
            settings.set_boolean(config::WINDOW_MAXIMIZED_KEY, self.is_maximized()),
        ] {
            if let Err(error) = result {
                crate::debug::log("settings", &format!("WindowStateSaveFailed error={error}"));
            }
        }
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

    pub(crate) fn flush_persistent_state(&self) {
        self.flush_current_drafts();
        self.save_window_state();
    }

    fn save_current_drafts(&self) {
        let changed = self.update_current_drafts();
        if draft_persist_required(changed, self.imp().draft_persist_pending.get()) {
            self.persist_drafts();
        }
    }

    fn update_current_drafts(&self) -> bool {
        let Some(channel_id) = self.visible_channel_id() else {
            return false;
        };
        let Some(channel_key) = self.draft_key(&channel_id, None) else {
            return false;
        };
        let thread_key = self
            .selected_thread_ts()
            .and_then(|thread_ts| self.draft_key(&channel_id, Some(&thread_ts)));
        let message_text = self.composer_canonical_text(ComposerTarget::Message);
        let thread_text = self.composer_canonical_text(ComposerTarget::Thread);
        {
            let mut drafts = self.imp().drafts.borrow_mut();
            let mut changed = drafts.upsert(channel_key, &message_text);
            if let Some(thread_key) = thread_key {
                changed |= drafts.upsert(thread_key, &thread_text);
            }
            changed
        }
    }

    fn persist_drafts(&self) {
        let Some(settings) = self.imp().settings.borrow().clone() else {
            return;
        };
        if let Err(error) = DraftSettings::new(settings).save(&self.imp().drafts.borrow()) {
            self.imp().draft_persist_pending.set(true);
            crate::debug::log("drafts", &format!("failed to persist drafts: {error}"));
            return;
        }
        self.imp().draft_persist_pending.set(false);
    }

    fn schedule_draft_persist(&self) {
        self.imp().draft_persist_pending.set(true);
        let weak_window = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(window) = weak_window.upgrade() {
                if window.imp().draft_persist_pending.get() {
                    window.persist_drafts();
                }
            }
        });
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
        self.set_composer_canonical_text(ComposerTarget::Message, &text);
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
        self.set_composer_canonical_text(ComposerTarget::Thread, &text);
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
                self.composer_canonical_text(ComposerTarget::Thread)
            } else {
                self.composer_canonical_text(ComposerTarget::Message)
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
            self.schedule_draft_persist();
        }
        if current_key.as_ref() == Some(&key) {
            if thread_ts.is_some() {
                self.set_composer_canonical_text(ComposerTarget::Thread, "");
            } else {
                self.set_composer_canonical_text(ComposerTarget::Message, "");
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
                self.composer_canonical_text(ComposerTarget::Thread)
            } else {
                self.composer_canonical_text(ComposerTarget::Message)
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
            self.schedule_draft_persist();
        }
        if current_text.is_some() {
            if thread_ts.is_some() {
                self.set_composer_canonical_text(ComposerTarget::Thread, "");
            } else {
                self.set_composer_canonical_text(ComposerTarget::Message, "");
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
        self.add_window_action("change-status", |window| window.show_change_status_dialog());
        self.add_window_action("new-message", |window| window.show_new_message_picker());
        self.add_window_action("new-channel", |window| window.show_new_channel_dialog());
        self.add_window_action("go-back", |window| window.go_back());
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

    fn composer_completion(&self, target: ComposerTarget) -> &RefCell<Option<ComposerCompletion>> {
        match target {
            ComposerTarget::Message => &self.imp().message_composer_completion,
            ComposerTarget::Thread => &self.imp().thread_composer_completion,
        }
    }

    fn composer_mentions(&self, target: ComposerTarget) -> &RefCell<Vec<ComposerMentionMark>> {
        match target {
            ComposerTarget::Message => &self.imp().message_mentions,
            ComposerTarget::Thread => &self.imp().thread_mentions,
        }
    }

    fn ensure_composer_mention_tag(&self, target: ComposerTarget) {
        const TAG_NAME: &str = "composer-mention";
        let buffer = self.composer_text_view(target).buffer();
        let table = buffer.tag_table();
        if table.lookup(TAG_NAME).is_none() {
            let tag = gtk::TextTag::builder().name(TAG_NAME).weight(600).build();
            table.add(&tag);
        }
    }

    fn clear_composer_mentions(&self, target: ComposerTarget) {
        let buffer = self.composer_text_view(target).buffer();
        for mention in self.composer_mentions(target).borrow_mut().drain(..) {
            if !mention.start.is_deleted() {
                buffer.delete_mark(&mention.start);
            }
            if !mention.end.is_deleted() {
                buffer.delete_mark(&mention.end);
            }
        }
    }

    fn add_composer_mention(&self, target: ComposerTarget, span: MentionSpan) {
        const TAG_NAME: &str = "composer-mention";
        self.ensure_composer_mention_tag(target);
        let buffer = self.composer_text_view(target).buffer();
        let start = buffer.iter_at_offset(span.start as i32);
        let end = buffer.iter_at_offset(span.end as i32);
        buffer.apply_tag_by_name(TAG_NAME, &start, &end);
        self.composer_mentions(target)
            .borrow_mut()
            .push(ComposerMentionMark {
                // Right gravity keeps text inserted immediately before the
                // mention outside its semantic span.
                start: buffer.create_mark(None, &start, false),
                // Left gravity keeps text inserted immediately after the
                // mention outside its semantic span.
                end: buffer.create_mark(None, &end, true),
                user_id: span.user_id,
                label: span.label,
            });
    }

    fn composer_mention_spans(&self, target: ComposerTarget) -> Vec<MentionSpan> {
        const TAG_NAME: &str = "composer-mention";
        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let text = text_view_text(&text_view);
        let characters = text.chars().collect::<Vec<_>>();
        let mut spans = Vec::new();
        self.composer_mentions(target)
            .borrow_mut()
            .retain(|mention| {
                if mention.start.is_deleted() || mention.end.is_deleted() {
                    return false;
                }
                let start = buffer.iter_at_mark(&mention.start).offset().max(0) as usize;
                let end = buffer.iter_at_mark(&mention.end).offset().max(0) as usize;
                let valid = start < end
                    && end <= characters.len()
                    && characters[start..end].iter().collect::<String>() == mention.label;
                if valid {
                    spans.push(MentionSpan {
                        start,
                        end,
                        user_id: mention.user_id.clone(),
                        label: mention.label.clone(),
                    });
                    true
                } else {
                    let tag_start = start.min(end).min(characters.len());
                    let tag_end = start.max(end).min(characters.len());
                    let start_iter = buffer.iter_at_offset(tag_start as i32);
                    let end_iter = buffer.iter_at_offset(tag_end as i32);
                    buffer.remove_tag_by_name(TAG_NAME, &start_iter, &end_iter);
                    buffer.delete_mark(&mention.start);
                    buffer.delete_mark(&mention.end);
                    false
                }
            });
        spans
    }

    fn refresh_composer_mention_names(&self, target: ComposerTarget) {
        const TAG_NAME: &str = "composer-mention";
        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let mut changed = false;

        loop {
            let update = {
                let names = self.imp().user_names.borrow();
                let mentions = self.composer_mentions(target).borrow();
                let text = text_view_text(&text_view);
                let characters = text.chars().collect::<Vec<_>>();

                mentions.iter().enumerate().find_map(|(index, mention)| {
                    if mention.start.is_deleted() || mention.end.is_deleted() {
                        return None;
                    }
                    let display_name = names
                        .get(&mention.user_id)
                        .map(String::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())?;
                    let label = format!("@{display_name}");
                    if label == mention.label {
                        return None;
                    }
                    let start = buffer.iter_at_mark(&mention.start).offset().max(0) as usize;
                    let end = buffer.iter_at_mark(&mention.end).offset().max(0) as usize;
                    (start < end
                        && end <= characters.len()
                        && characters[start..end].iter().collect::<String>() == mention.label)
                        .then(|| (index, start, end, mention.user_id.clone(), label))
                })
            };
            let Some((index, start, end, user_id, label)) = update else {
                break;
            };

            let mention = self.composer_mentions(target).borrow_mut().remove(index);
            let mut start_iter = buffer.iter_at_offset(start as i32);
            let mut end_iter = buffer.iter_at_offset(end as i32);
            buffer.remove_tag_by_name(TAG_NAME, &start_iter, &end_iter);
            buffer.delete_mark(&mention.start);
            buffer.delete_mark(&mention.end);
            buffer.begin_user_action();
            buffer.delete(&mut start_iter, &mut end_iter);
            buffer.insert(&mut start_iter, &label);
            buffer.end_user_action();
            changed = true;
            self.add_composer_mention(
                target,
                MentionSpan {
                    start,
                    end: start + label.chars().count(),
                    user_id,
                    label,
                },
            );
        }
        if changed {
            self.refresh_composer_completion(target);
        }
    }

    fn composer_range_intersects_mention(
        &self,
        target: ComposerTarget,
        start: usize,
        end: usize,
    ) -> bool {
        self.composer_mention_spans(target)
            .iter()
            .any(|span| start < span.end && span.start < end)
    }

    fn composer_canonical_text(&self, target: ComposerTarget) -> String {
        let text = text_view_text(&self.composer_text_view(target));
        serialize_composer_mentions(&text, &self.composer_mention_spans(target))
    }

    fn set_composer_canonical_text(&self, target: ComposerTarget, text: &str) {
        let names = self.imp().user_names.borrow();
        let hydrated = hydrate_composer_mentions(text, &names);
        drop(names);
        self.clear_composer_mentions(target);
        self.composer_text_view(target)
            .buffer()
            .set_text(&hydrated.text);
        for mention in hydrated.mentions {
            self.add_composer_mention(target, mention);
        }
    }

    fn setup_composer_completion(&self, target: ComposerTarget) {
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
        list.update_property(&[gtk::accessible::Property::Label("Composer suggestions")]);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_min_content_width(280);
        scroller.set_max_content_height(320);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&list));
        popover.set_child(Some(&scroller));

        let completion = ComposerCompletion {
            popover: popover.clone(),
            list: list.clone(),
            entries: Vec::new(),
            token: None,
            directory_checked_mention_start: None,
        };
        *self.composer_completion(target).borrow_mut() = Some(completion);

        let weak_window = self.downgrade();
        list.connect_row_activated(move |_, _| {
            if let Some(window) = weak_window.upgrade() {
                window.accept_composer_completion(target);
            }
        });

        let weak_window = self.downgrade();
        text_view.buffer().connect_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.refresh_composer_completion(target);
            }
        });

        let weak_window = self.downgrade();
        text_view.buffer().connect_mark_set(move |_, _, mark| {
            if mark.name().as_deref() == Some("insert") {
                if let Some(window) = weak_window.upgrade() {
                    window.refresh_composer_completion(target);
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
                        window.dismiss_composer_completion(target);
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
                    window.handle_composer_completion_key(target, key, state)
                })
        });
        text_view.add_controller(controller);
    }

    fn should_check_user_directory_for_mention(
        &self,
        target: ComposerTarget,
        mention_start: Option<usize>,
    ) -> bool {
        let mut completion_ref = self.composer_completion(target).borrow_mut();
        let Some(completion) = completion_ref.as_mut() else {
            return false;
        };
        let (next_checked_start, should_check) = composer_directory_check_transition(
            mention_start,
            completion.directory_checked_mention_start,
        );
        completion.directory_checked_mention_start = next_checked_start;
        should_check
    }

    fn reset_composer_user_directory_checks(&self) {
        for target in COMPOSER_TARGETS {
            if let Some(completion) = self.composer_completion(target).borrow_mut().as_mut() {
                completion.directory_checked_mention_start = None;
            }
        }
    }

    fn refresh_composer_completion(&self, target: ComposerTarget) {
        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let text = text_view_text(&text_view);
        let caret = buffer.cursor_position().max(0) as usize;
        let mention_token = mention_token_at_caret(&text, caret).filter(|token| {
            !self.composer_range_intersects_mention(target, token.start, token.end)
        });
        if self.should_check_user_directory_for_mention(
            target,
            mention_token.as_ref().map(|token| token.start),
        ) {
            // Cached candidates remain immediately available. The runtime decides
            // whether the directory is old enough to require network activity.
            self.request_user_directory();
        }
        let (token, entries) = if let Some(token) = mention_token {
            let candidates = mention_candidates(&self.imp().discovered_users.borrow());
            let entries = search_mention_candidates(&candidates, &token.query, 10)
                .into_iter()
                .map(ComposerCompletionEntry::Mention)
                .collect();
            (Some(ComposerCompletionToken::Mention(token)), entries)
        } else {
            let token = emoji_token_at_caret(&text, caret);
            let entries = token.as_ref().map_or_else(Vec::new, |token| {
                let custom_emojis = self.imp().custom_emojis.borrow();
                let catalog = EmojiCatalog::new(&custom_emojis);
                EmojiPickerModel::new(catalog.entries())
                    .search(&token.query)
                    .into_iter()
                    .take(10)
                    .map(ComposerCompletionEntry::Emoji)
                    .collect::<Vec<_>>()
            });
            (token.map(ComposerCompletionToken::Emoji), entries)
        };
        let is_person_completion = matches!(token, Some(ComposerCompletionToken::Mention(_)));

        let mut completion_ref = self.composer_completion(target).borrow_mut();
        let Some(completion) = completion_ref.as_mut() else {
            return;
        };
        completion.token = token;
        completion.entries = entries;

        while let Some(child) = completion.list.first_child() {
            completion.list.remove(&child);
        }
        if completion.entries.is_empty() {
            text_view.update_property(&[gtk::accessible::Property::Description("")]);
            completion.popover.popdown();
            return;
        }

        completion
            .list
            .update_property(&[gtk::accessible::Property::Label(if is_person_completion {
                "Person suggestions"
            } else {
                "Emoji suggestions"
            })]);
        for entry in &completion.entries {
            completion.list.append(&composer_completion_row(entry));
        }
        completion
            .list
            .select_row(completion.list.row_at_index(0).as_ref());
        text_view.update_property(&[gtk::accessible::Property::Description(
            &composer_completion_description(&completion.entries[0], 0, completion.entries.len()),
        )]);
        let insert = buffer.iter_at_offset(buffer.cursor_position());
        completion
            .popover
            .set_pointing_to(Some(&text_view.iter_location(&insert)));
        completion.popover.popup();
        record_test_composer_completion_ready(target, completion);
    }

    fn dismiss_composer_completion(&self, target: ComposerTarget) {
        let mut completion_ref = self.composer_completion(target).borrow_mut();
        if let Some(completion) = completion_ref.as_mut() {
            completion.token = None;
            completion.entries.clear();
            completion.popover.popdown();
        }
        self.composer_text_view(target)
            .update_property(&[gtk::accessible::Property::Description("")]);
    }

    fn move_composer_completion_selection(
        &self,
        target: ComposerTarget,
        movement: EmojiPickerMove,
    ) {
        let completion_ref = self.composer_completion(target).borrow();
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
            if let Some(entry) = completion.entries.get(next) {
                self.composer_text_view(target).update_property(&[
                    gtk::accessible::Property::Description(&composer_completion_description(
                        entry,
                        next,
                        completion.entries.len(),
                    )),
                ]);
            }
            record_test_composer_completion_ready(target, completion);
        }
    }

    fn accept_composer_completion(&self, target: ComposerTarget) {
        let selection = {
            let completion_ref = self.composer_completion(target).borrow();
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
            (token, entry.clone())
        };

        let text_view = self.composer_text_view(target);
        let buffer = text_view.buffer();
        let text = text_view_text(&text_view);
        match selection {
            (ComposerCompletionToken::Emoji(token), ComposerCompletionEntry::Emoji(entry)) => {
                let (updated, caret) = replace_emoji_token(&text, &token, &entry.name);
                let replacement = updated
                    .chars()
                    .skip(token.start)
                    .take(caret.saturating_sub(token.start))
                    .collect::<String>();
                let mut start = buffer.iter_at_offset(token.start as i32);
                let mut end = buffer.iter_at_offset(token.end as i32);
                buffer.begin_user_action();
                buffer.delete(&mut start, &mut end);
                buffer.insert(&mut start, &replacement);
                buffer.place_cursor(&buffer.iter_at_offset(caret as i32));
                buffer.end_user_action();
                record_test_composer_completion(
                    self.imp(),
                    target,
                    TestComposerCompletion::Emoji(&entry.name),
                );
            }
            (
                ComposerCompletionToken::Mention(token),
                ComposerCompletionEntry::Mention(candidate),
            ) => {
                let insertion = replace_mention_token(&text, &token, &candidate);
                let old_length = text.chars().count();
                let new_length = insertion.text.chars().count();
                let replacement_length =
                    new_length.saturating_sub(old_length.saturating_sub(token.end - token.start));
                let replacement = insertion
                    .text
                    .chars()
                    .skip(token.start)
                    .take(replacement_length)
                    .collect::<String>();
                let mut start = buffer.iter_at_offset(token.start as i32);
                let mut end = buffer.iter_at_offset(token.end as i32);
                buffer.begin_user_action();
                buffer.delete(&mut start, &mut end);
                buffer.insert(&mut start, &replacement);
                buffer.place_cursor(&buffer.iter_at_offset(insertion.caret as i32));
                buffer.end_user_action();
                self.add_composer_mention(target, insertion.span);
                let serialized = self.composer_canonical_text(target);
                record_test_composer_completion(
                    self.imp(),
                    target,
                    TestComposerCompletion::Mention {
                        user_id: &candidate.user_id,
                        serialized: &serialized,
                    },
                );
            }
            _ => return,
        }
        self.dismiss_composer_completion(target);
        text_view.grab_focus();
    }

    fn handle_composer_completion_key(
        &self,
        target: ComposerTarget,
        key: gtk::gdk::Key,
        state: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        let is_open = {
            let completion_ref = self.composer_completion(target).borrow();
            completion_ref
                .as_ref()
                .is_some_and(|completion| completion.popover.is_visible())
        };
        if !is_open {
            return glib::Propagation::Proceed;
        }

        match completion_key_action(key, state) {
            CompletionKeyAction::Previous => {
                self.move_composer_completion_selection(target, EmojiPickerMove::Previous)
            }
            CompletionKeyAction::Next => {
                self.move_composer_completion_selection(target, EmojiPickerMove::Next)
            }
            CompletionKeyAction::Accept => self.accept_composer_completion(target),
            CompletionKeyAction::Dismiss => self.dismiss_composer_completion(target),
            CompletionKeyAction::Ignore => return glib::Propagation::Proceed,
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
                    self.set_status(&status);
                }
            }
            RuntimeEventKind::Error(error) => {
                if is_user_directory_request(&meta.context) {
                    self.imp().user_directory_request_in_flight.set(false);
                    self.imp().user_directory_failed.set(true);
                    self.reset_composer_user_directory_checks();
                    self.refresh_open_people_picker();
                }
                if failed_user_directory_request(
                    &meta.context,
                    self.imp().discovered_users.borrow().is_empty(),
                ) {
                    self.imp().user_directory_loaded.set(false);
                }
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
                }
            }
            RuntimeEventKind::WorkspacePatch(patch) => {
                self.apply_workspace_patch(&patch);
            }
            RuntimeEventKind::ConversationsSynchronized => {
                if !self.imp().connect_requested.get() {
                    if conversation_sync_completion_needs_catalog_sync(
                        self.imp().workspace_ready.get(),
                    ) {
                        self.sync_conversations_from_catalog();
                    }
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
                self.imp().user_directory_request_in_flight.set(false);
                self.imp().user_directory_loaded.set(true);
                self.imp().user_directory_failed.set(false);
                self.replace_user_directory_identity_maps(&users);
                *self.imp().discovered_users.borrow_mut() = users;
                self.resolve_deferred_user_ids();
                self.refresh_open_conversation_picker();
                self.refresh_open_people_picker();
                for target in COMPOSER_TARGETS {
                    self.refresh_composer_completion(target);
                }
            }
            RuntimeEventKind::ConversationOpened { channel_id } => {
                let title = self.conversation_title(&channel_id);
                self.select_conversation(&channel_id, &title);
            }
            RuntimeEventKind::ConversationUpdated { channel_id: _ } => {
                self.refresh_current_conversation_title();
                self.set_status(&gettext("People added"));
            }
            RuntimeEventKind::ConversationStarUpdated {
                channel_id: _,
                starred,
            } => {
                self.set_status(&gettext(if starred {
                    "Conversation starred"
                } else {
                    "Conversation unstarred"
                }));
            }
            RuntimeEventKind::CurrentUserStatusUpdated { user_id, status } => {
                self.imp().pending_status_update.borrow_mut().take();
                let cleared = status.is_none();
                let mut statuses = self.imp().user_statuses.borrow_mut();
                if let Some(status) = status {
                    Arc::make_mut(&mut statuses).insert(user_id.clone(), status);
                } else {
                    Arc::make_mut(&mut statuses).remove(&user_id);
                }
                drop(statuses);
                self.user_statuses_changed(vec![user_id]);
                self.set_status(&gettext(if cleared {
                    "Status cleared"
                } else {
                    "Status updated"
                }));
            }
            RuntimeEventKind::ConversationLeft { channel_id: _ } => {
                self.set_status(&gettext("Left channel"));
            }
            RuntimeEventKind::AttentionNotificationCandidate {
                channel_id,
                message,
                decision,
            } => self.handle_attention_notification_candidate(&channel_id, &message, &decision),
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
                    if outcome.render {
                        let rendered_messages = self
                            .imp()
                            .workspace
                            .view
                            .borrow()
                            .snapshot()
                            .channel_messages;
                        self.populate_history_with_scroll(
                            &channel_id,
                            rendered_messages,
                            timeline_scroll_behavior(
                                outcome
                                    .scroll
                                    .unwrap_or(WorkspaceScrollBehavior::StickToBottom),
                            ),
                        );
                    }
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
                if let ThreadApplyOutcome::Applied { scroll, render } = outcome {
                    if render {
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
                    }
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
            RuntimeEventKind::FileLoaded {
                file,
                share_requested,
            } => {
                let visible = self
                    .imp()
                    .workspace
                    .view
                    .borrow_mut()
                    .apply_files(vec![*file]);
                if visible {
                    let files = self.imp().workspace.view.borrow().files().to_vec();
                    self.populate_files(files);
                    if share_requested {
                        self.set_status(&gettext("Sharing existing files is not supported yet."));
                    }
                }
            }
            RuntimeEventKind::SavedItemsLoaded(items) => {
                let visible = self.imp().workspace.view.borrow_mut().apply_saved(items);
                if visible {
                    let items = self.imp().workspace.view.borrow().saved_items().to_vec();
                    self.populate_saved_items(items);
                }
            }
            RuntimeEventKind::RealtimeStatusChanged(status) => self.set_realtime_status(status),
            RuntimeEventKind::SocketModeEvent { event, attention } => {
                self.handle_socket_mode_event(event, attention)
            }
            RuntimeEventKind::Huddle(event) => self.handle_huddle_event(event),
            RuntimeEventKind::UserLoaded {
                user_id,
                display_name,
                full_name,
                avatar_url,
                status,
            } => {
                self.populate_user_names(HashMap::from([(user_id.clone(), display_name)]));
                if let Some(full_name) = full_name {
                    self.populate_user_full_names(HashMap::from([(user_id.clone(), full_name)]));
                }
                if let Some(avatar_url) = avatar_url {
                    self.populate_user_avatar_urls(HashMap::from([(user_id.clone(), avatar_url)]));
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
            RuntimeEventKind::UserAvatarUrlsLoaded(urls) => self.populate_user_avatar_urls(urls),
            RuntimeEventKind::UserSearchAliasesLoaded(aliases) => {
                *self.imp().user_search_aliases.borrow_mut() = aliases;
                self.queue_ui_invalidations(UiInvalidations::SIDEBAR | UiInvalidations::PICKER);
            }
            RuntimeEventKind::UserStatusesLoaded {
                statuses,
                replace_existing,
                preserve_user_ids,
            } => {
                self.apply_user_statuses_snapshot(statuses, replace_existing, &preserve_user_ids);
            }
            RuntimeEventKind::UserGroupsLoaded { names, members } => {
                self.populate_user_groups(names, members);
            }
            RuntimeEventKind::EmojiCatalogLoaded(emojis) => self.replace_custom_emojis(emojis),
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
            RuntimeEventKind::MessagePermalinkResolved { handoff } => {
                match open_resolved_handoff(&SystemExternalOpener, &handoff) {
                    Ok(()) => match handoff.provenance {
                        HandoffProvenance::Authoritative
                        | HandoffProvenance::CachedAuthoritative => {
                            self.set_status("Opened in Slack to complete the action")
                        }
                        HandoffProvenance::ConstructedFallback(_) => {
                            self.set_status("Opened a fallback Slack link to complete the action")
                        }
                    },
                    Err(error) => {
                        self.set_status(&format!("Failed to open message in Slack: {error}"))
                    }
                }
            }
            RuntimeEventKind::MessagePosted {
                channel_id,
                message,
            } => {
                self.set_status("Message sent");
                let thread_ts = posted_message_thread_ts(&meta.context, &channel_id, &message);
                let mut message = *message;
                if let Some(thread_ts) = thread_ts.as_deref() {
                    message.thread_ts = Some(thread_ts.to_string());
                }
                self.complete_submitted_draft(&channel_id, thread_ts.as_deref());
                if thread_ts.is_some() {
                    self.imp().thread_send_button.set_sensitive(true);
                } else {
                    self.imp().send_button.set_sensitive(true);
                }
                let outcome = self.apply_timeline_message(
                    &channel_id,
                    &message,
                    RealtimeMessageKind::Posted,
                    false,
                    Some(TimelineMessageArrival::Sent),
                );
                if outcome.refresh_unreads {
                    self.populate_unreads(self.unread_items());
                } else {
                    self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
                }
            }
            RuntimeEventKind::ReactionUpdated {
                channel_id,
                ts,
                name,
                added,
                thread_ts,
            } => {
                self.set_status("Reaction updated");
                let current_user_id = self.imp().current_user_id.borrow().clone();
                if let Some(update) = local_reaction_update(
                    &channel_id,
                    &ts,
                    &name,
                    added,
                    current_user_id.as_deref(),
                ) {
                    self.apply_reaction_update(update);
                } else {
                    self.reload_after_message(&channel_id, thread_ts.as_deref());
                }
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
            self.refresh_workspace_title_status();
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

    fn apply_realtime_message_patch(&self, request: RealtimeMessagePatch<'_>) {
        let control_surface = match request.surface {
            TimelineSurface::Main => TimelineSurfaceId::Main,
            TimelineSurface::Thread => TimelineSurfaceId::Thread,
        };
        if let Ok(target) = MessageRef::new(request.channel_id, request.message.ts.clone()) {
            let mut registry = self.imp().message_control_registry.borrow_mut();
            match request.kind {
                RealtimeMessageKind::Posted | RealtimeMessageKind::Changed => {
                    let _ = registry.replace_message(control_surface, target);
                }
                RealtimeMessageKind::Deleted => {
                    registry.remove_message(control_surface, &target);
                }
            }
        }
        let patch = match request.kind {
            RealtimeMessageKind::Posted => {
                let mut context = self.message_patch_context(request.thread_ts, request.message);
                if request.unread_start {
                    context.first_unread_ts = Some(request.message.ts.clone());
                }
                message_html::insert_message_patch(
                    request.channel_id,
                    request.message,
                    &context,
                    TimelineInsertPosition::Append,
                    request.arrival,
                )
            }
            RealtimeMessageKind::Changed => message_html::replace_message_patch(
                request.channel_id,
                request.message,
                &self.message_patch_context(request.thread_ts, request.message),
                request.arrival,
            ),
            // Slack retains a tombstone for deleted messages. Replacing the existing
            // article keeps the incremental path consistent with a complete render.
            RealtimeMessageKind::Deleted => message_html::replace_message_patch(
                request.channel_id,
                request.message,
                &self.message_patch_context(request.thread_ts, request.message),
                None,
            ),
        };
        self.apply_timeline_patch(request.surface, patch, request.fallback);
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
        imp.browser_session_howto_link.set_visible(browser_session);

        if browser_session {
            imp.auth_intro_label.set_label(
                "Paste browser-session credentials. They will be stored in the system keyring.",
            );
            imp.setup_hint_label.set_visible(true);
            imp.setup_hint_label.set_label(
                "Paste xoxc and xoxd from the same browser. Enterprise Slack may require its exact navigator.userAgent; if that still fails, use OAuth because Conduit cannot imitate browser TLS.",
            );
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
        self.send_command(RuntimeCommand::RefreshConversations);
    }

    fn current_navigation_target(&self) -> Option<MainNavigationTarget> {
        let view = self.imp().workspace.view.borrow();
        match view.main_view() {
            MainMessageView::Conversation => view
                .visible_channel_id()
                .map(|channel_id| MainNavigationTarget::Conversation(channel_id.to_string())),
            MainMessageView::Unreads => Some(MainNavigationTarget::Unreads),
            MainMessageView::Threads => Some(MainNavigationTarget::Threads),
            MainMessageView::Search => Some(MainNavigationTarget::Search),
            MainMessageView::Files => Some(MainNavigationTarget::Files),
            MainMessageView::Saved => Some(MainNavigationTarget::Saved),
            MainMessageView::Placeholder => None,
        }
    }

    fn record_navigation(&self, target: &MainNavigationTarget) {
        let imp = self.imp();
        imp.profile_visible.set(false);
        if imp.restoring_navigation.get() {
            return;
        }
        let Some(current) = self.current_navigation_target() else {
            return;
        };
        let mut history = imp.navigation_history.borrow_mut();
        remember_navigation(&mut history, current, target);
        drop(history);
        self.sync_back_button();
    }

    fn sync_back_button(&self) {
        let imp = self.imp();
        imp.navigation_back_button.set_sensitive(
            imp.profile_visible.get() || !imp.navigation_history.borrow().is_empty(),
        );
    }

    fn go_back(&self) {
        let imp = self.imp();
        if imp.profile_visible.replace(false) {
            imp.pending_profile_user_id.borrow_mut().take();
            self.queue_ui_invalidations(UiInvalidations::MAIN);
            self.sync_back_button();
            return;
        }
        let Some(target) = imp.navigation_history.borrow_mut().pop() else {
            self.sync_back_button();
            return;
        };
        imp.restoring_navigation.set(true);
        match target {
            MainNavigationTarget::Conversation(channel_id) => {
                let title = self.conversation_title(&channel_id);
                self.select_conversation(&channel_id, &title);
            }
            MainNavigationTarget::Unreads => self.show_unreads(),
            MainNavigationTarget::Threads => self.show_threads(),
            MainNavigationTarget::Search => {
                imp.workspace.view.borrow_mut().show_search();
                self.render_closed_thread();
                self.render_conversations();
                self.rerender_current_main_messages();
                imp.workspace_split.set_show_content(true);
            }
            MainNavigationTarget::Files => self.show_files(),
            MainNavigationTarget::Saved => self.show_later(),
        }
        imp.restoring_navigation.set(false);
        self.sync_back_button();
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
        self.record_navigation(&MainNavigationTarget::Unreads);
        self.flush_current_drafts();
        self.imp().workspace.view.borrow_mut().show_unreads();
        self.render_closed_thread();
        let items = self.unread_items();
        self.populate_unreads(items);
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_threads(&self) {
        self.record_navigation(&MainNavigationTarget::Threads);
        self.flush_current_drafts();
        self.imp().workspace.view.borrow_mut().show_threads();
        self.render_closed_thread();
        self.populate_threads();
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_files(&self) {
        self.start_files_surface(&gettext("Loading files"));
        self.send_command(RuntimeCommand::LoadFiles);
    }

    fn show_slack_file(&self, file_id: &str, share_requested: bool) {
        self.start_files_surface(&gettext("Loading file"));
        self.send_command(RuntimeCommand::LoadFile {
            file_id: file_id.to_string(),
            share_requested,
        });
    }

    fn start_files_surface(&self, loading_message: &str) {
        self.record_navigation(&MainNavigationTarget::Files);
        self.flush_current_drafts();
        let title = gettext("Files");
        self.imp().workspace.view.borrow_mut().start_files();
        self.render_closed_thread();
        self.imp().message_title.set_title(&title);
        self.render_conversations();
        self.load_message_html(&message_html::placeholder_document(&title, loading_message));
        self.imp().workspace_split.set_show_content(true);
    }

    fn show_later(&self) {
        self.record_navigation(&MainNavigationTarget::Saved);
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
        self.record_navigation(&MainNavigationTarget::Search);
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
        let text = self
            .composer_canonical_text(ComposerTarget::Message)
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }

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
        let text = self
            .composer_canonical_text(ComposerTarget::Thread)
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }

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
        let initial_comment = self
            .composer_canonical_text(ComposerTarget::Message)
            .trim()
            .to_string();

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
            let target = if thread {
                ComposerTarget::Thread
            } else {
                ComposerTarget::Message
            };
            let initial_comment = window.composer_canonical_text(target).trim().to_string();
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
        self.select_conversation_target(&channel_id, &title, Some(location.message_ts()));
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
        self.set_composer_canonical_text(ComposerTarget::Thread, "");
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

    fn show_user_profile(&self, user_id: &str) {
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return;
        }
        self.close_media_viewer();
        self.imp().profile_visible.set(true);
        self.sync_back_button();
        *self.imp().pending_profile_user_id.borrow_mut() = Some(user_id.to_string());
        self.imp().message_title.set_title(&gettext("Profile"));
        self.load_message_html(&message_html::placeholder_document(
            &gettext("Profile"),
            &gettext("Loading profile"),
        ));
        self.imp().workspace_split.set_show_content(true);
        self.send_command(RuntimeCommand::LoadUserProfile {
            user_id: user_id.to_string(),
        });
    }

    fn handle_message_action_url(&self, url: &url::Url) -> bool {
        match url.host_str() {
            Some("timeline-positioned" | "timeline-interacted") => {
                match timeline_lifecycle_action(url) {
                    Some(TimelineLifecycleAction::Positioned(generation)) => {
                        let reconcile = {
                            let mut opening = self.imp().conversation_opening.borrow_mut();
                            opening.commit_position(generation)
                                && opening.take_pending_reconciliation(generation)
                        };
                        if reconcile {
                            self.reconcile_current_conversation_snapshot();
                        }
                    }
                    Some(TimelineLifecycleAction::Interacted(generation)) => {
                        let reconcile = {
                            let mut opening = self.imp().conversation_opening.borrow_mut();
                            opening.note_user_interaction(generation)
                                && opening.take_pending_reconciliation(generation)
                        };
                        if reconcile {
                            self.reconcile_current_conversation_snapshot();
                        }
                    }
                    None => {}
                }
                true
            }
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
            Some("message-control") => {
                let query = url.query_pairs().collect::<Vec<_>>();
                let Some(handle) =
                    (query.len() == 1 && query[0].0 == "id").then(|| query[0].1.to_string())
                else {
                    self.set_status("Message action is no longer available");
                    return true;
                };
                let target = self
                    .imp()
                    .message_control_registry
                    .borrow_mut()
                    .activate_token(&handle);
                let Ok(target) = target else {
                    self.set_status("Message action is no longer available");
                    return true;
                };
                if self
                    .find_message(target.channel_id(), target.timestamp())
                    .is_none()
                {
                    self.set_status("This message changed; try again");
                    return true;
                }
                self.set_status("Opening message in Slack");
                self.send_command(RuntimeCommand::ResolveMessagePermalink {
                    channel_id: target.channel_id().to_string(),
                    ts: target.timestamp().to_string(),
                });
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
            Some("channel") => {
                if let Some(channel_id) = query_param(url, "channel") {
                    self.open_channel_reference(&channel_id);
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
                    self.show_user_profile(&user_id);
                }
                true
            }
            Some("profile-close") => {
                self.imp().profile_visible.set(false);
                self.imp().pending_profile_user_id.borrow_mut().take();
                self.queue_ui_invalidations(UiInvalidations::MAIN);
                self.sync_back_button();
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

        let text = message.visible_text();
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
        self.withdraw_huddle_notification();
        let imp = self.imp();
        *imp.huddle_snapshot.borrow_mut() = HuddleSnapshot::default();
        imp.huddle_devices.borrow_mut().clear();
        if let Some(preflight) = imp.huddle_preflight_dialog.borrow_mut().take() {
            preflight.dialog.force_close();
        }
        let people_picker = imp.people_picker_view.borrow_mut().take();
        if let Some(view) = people_picker {
            view.dialog.close();
        }
        let status_dialog = imp.status_dialog.borrow_mut().take();
        if let Some(state) = status_dialog {
            state.dialog.force_close();
        }
        imp.pending_status_update.borrow_mut().take();
        imp.huddle_revealer.set_reveal_child(false);
        imp.workspace.reset();
        imp.conversation_opening.borrow_mut().reset();
        imp.message_control_registry.borrow_mut().reset_session();
        self.set_realtime_status(RealtimeStatus::default());
        imp.navigation_history.borrow_mut().clear();
        imp.restoring_navigation.set(false);
        imp.profile_visible.set(false);
        self.sync_back_button();
        *imp.current_user_id.borrow_mut() = None;
        *imp.workspace_id.borrow_mut() = None;
        *imp.workspace_team_id.borrow_mut() = None;
        imp.workspace_ready.set(false);
        imp.initial_sync_complete.set(false);
        imp.pending_message_notifications.borrow_mut().clear();
        imp.pending_sent_drafts.borrow_mut().clear();
        imp.pending_upload_drafts.borrow_mut().clear();
        imp.discovered_channels.borrow_mut().clear();
        imp.discovered_users.borrow_mut().clear();
        imp.user_directory_loaded.set(false);
        imp.user_directory_failed.set(false);
        imp.sidebar_row_actions.borrow_mut().clear();
        imp.sidebar_section_actions.borrow_mut().clear();
        *imp.user_names.borrow_mut() = Arc::default();
        *imp.user_full_names.borrow_mut() = Arc::default();
        *imp.user_avatar_urls.borrow_mut() = Arc::default();
        imp.user_search_aliases.borrow_mut().clear();
        *imp.user_statuses.borrow_mut() = Arc::default();
        imp.status_expiry_generation
            .set(imp.status_expiry_generation.get().saturating_add(1));
        *imp.user_group_names.borrow_mut() = Arc::default();
        *imp.user_group_members.borrow_mut() = Arc::default();
        imp.pending_user_ids.borrow_mut().clear();
        imp.deferred_user_ids.borrow_mut().clear();
        imp.user_directory_request_in_flight.set(false);
        *imp.workspace_name.borrow_mut() = None;
        *imp.workspace_url.borrow_mut() = None;
        *imp.sidebar_error.borrow_mut() = None;
        imp.image_assets.borrow_mut().clear();
        imp.pending_image_assets.borrow_mut().clear();
        imp.failed_image_assets.borrow_mut().clear();
        *imp.custom_emojis.borrow_mut() = Arc::default();
        self.set_composer_canonical_text(ComposerTarget::Message, "");
        self.set_composer_canonical_text(ComposerTarget::Thread, "");
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
        imp.workspace_title_label.set_subtitle("");
        imp.workspace_title_label.set_tooltip_text(None);
        imp.workspace_title_label
            .update_property(&[gtk::accessible::Property::Description("")]);
        imp.workspace_status_label.set_label("");
        imp.message_status_label.set_label("");
        imp.workspace_split.set_show_content(false);
        self.thread_pane().close();
        self.sync_workspace_chrome();
        self.clear_list(&imp.conversation_list);
        imp.sidebar_projection
            .borrow_mut()
            .reset(Vec::new())
            .expect("an empty sidebar projection should be valid");
        imp.sidebar_rows.borrow_mut().clear();
        imp.sidebar_row_actions.borrow_mut().clear();
        self.show_message_placeholder(&gettext("Select a conversation"));
    }

    fn show_workspace(&self, auth: AuthInfo) {
        *self.imp().workspace_id.borrow_mut() = workspace_identity(&auth);
        self.imp().workspace_ready.set(false);
        self.imp().initial_sync_complete.set(false);
        if let (Some(user_id), Some(user_name)) = (auth.user_id.as_deref(), auth.user.as_deref()) {
            let user_name = user_name.trim();
            if !user_name.is_empty() {
                Arc::make_mut(&mut self.imp().user_names.borrow_mut())
                    .insert(user_id.to_string(), user_name.to_string());
            }
        }
        *self.imp().current_user_id.borrow_mut() = auth.user_id.clone();
        *self.imp().workspace_team_id.borrow_mut() = auth.team_id.clone();
        *self.imp().workspace_url.borrow_mut() = auth.url.clone();
        self.imp().connect_button.set_sensitive(true);
        let workspace_name = auth
            .team
            .or(auth.team_id)
            .unwrap_or_else(|| "Slack".to_string());
        *self.imp().workspace_name.borrow_mut() = Some(workspace_name.clone());
        self.refresh_workspace_title_status();
        self.set_status("");
        self.imp().content_stack.set_visible_child_name("workspace");
        self.imp().workspace_split.set_show_content(false);
        self.sync_workspace_chrome();
        self.render_workspace_lifecycle();
        self.activate_pending_notification_target();
        self.activate_pending_slack_uris();
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
        let imp = self.imp();
        let lifecycle = imp.workspace.transition_lifecycle(event);
        imp.initial_sync_complete.set(initial_sync_completion(
            imp.initial_sync_complete.get(),
            lifecycle,
        ));
        self.render_workspace_lifecycle();
    }

    fn render_workspace_lifecycle(&self) {
        let imp = self.imp();
        let presentation = workspace_lifecycle_presentation(
            imp.workspace.lifecycle(),
            imp.workspace_id.borrow().is_some(),
            imp.initial_sync_complete.get(),
        );
        let status = gettext(presentation.status);
        imp.connection_label.set_label(&status);
        imp.workspace_status_label.set_label(&status);
        imp.workspace_split
            .set_sensitive(presentation.workspace_interactive);

        let (icon_name, tooltip) = match imp.workspace.lifecycle() {
            WorkspaceLifecycle::Ready => (
                match imp.realtime_status.get().phase {
                    RealtimePhase::Online => "network-wired-symbolic",
                    RealtimePhase::Connecting => "network-wireless-acquiring-symbolic",
                    RealtimePhase::Reconnecting | RealtimePhase::NotConfigured => {
                        "network-wired-offline-symbolic"
                    }
                    RealtimePhase::ConfigurationError => "dialog-warning-symbolic",
                },
                gettext(match imp.realtime_status.get().phase {
                    RealtimePhase::Online => "Realtime updates online",
                    RealtimePhase::Connecting => "Connecting realtime updates...",
                    RealtimePhase::Reconnecting => "Realtime connection interrupted; retrying...",
                    RealtimePhase::NotConfigured => "Realtime updates are not configured",
                    RealtimePhase::ConfigurationError => "Realtime updates could not be configured",
                }),
            ),
            WorkspaceLifecycle::Connecting | WorkspaceLifecycle::Syncing => (
                "network-wireless-acquiring-symbolic",
                gettext("Connecting to Slack..."),
            ),
            WorkspaceLifecycle::Disconnected | WorkspaceLifecycle::AuthenticationRequired => {
                ("network-wired-offline-symbolic", gettext("Disconnected"))
            }
            WorkspaceLifecycle::Degraded | WorkspaceLifecycle::StartupFailed => (
                "dialog-warning-symbolic",
                gettext("Connection degraded; retrying..."),
            ),
        };
        imp.connection_status_icon.set_icon_name(Some(icon_name));
        imp.connection_status_icon.set_tooltip_text(Some(&tooltip));

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

    fn replace_custom_emojis(&self, emojis: HashMap<String, String>) {
        *self.imp().custom_emojis.borrow_mut() = Arc::new(emojis);
        self.imp().reaction_emoji_picker_model.borrow_mut().take();
        if let Some(state) = self.imp().status_dialog.borrow().as_ref() {
            state
                .emoji_picker
                .refresh_catalog(&self.imp().custom_emojis.borrow());
            write_status_dialog_test_state(self, state);
        }
        self.queue_ui_invalidations(
            UiInvalidations::SIDEBAR
                | UiInvalidations::PICKER
                | UiInvalidations::TITLE
                | UiInvalidations::MAIN
                | UiInvalidations::THREAD,
        );
        for target in COMPOSER_TARGETS {
            self.refresh_composer_completion(target);
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
            RuntimeFailureRecovery::ConversationStar => self.set_status(error),
            RuntimeFailureRecovery::UserStatus => {
                let draft = self.imp().pending_status_update.borrow_mut().take();
                let error = current_user_status_error_message(failure);
                self.set_status(&error);
                if let Some(draft) = draft {
                    self.present_change_status_dialog(
                        draft.dialog_draft,
                        Some(&error),
                        draft.clearing,
                    );
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
        if self
            .imp()
            .user_avatar_urls
            .borrow()
            .values()
            .any(|url| url == key)
        {
            self.queue_ui_invalidations(UiInvalidations::MAIN | UiInvalidations::THREAD);
        }
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

    fn apply_workspace_patch(&self, patch: &crate::workspace_pipeline::WorkspacePatch) {
        let revision = patch.revision();
        let application = self.imp().workspace.apply_workspace_patch(patch);
        let Some(application) = application else {
            crate::debug::log(
                "ui",
                &format!(
                    "WorkspacePatchIgnored reason=stale revision={}",
                    revision.value()
                ),
            );
            return;
        };

        remove_patch_departures_from_discovery(
            &mut self.imp().discovered_channels.borrow_mut(),
            application.removals(),
        );
        for removal in application.removals() {
            if removal.was_visible() {
                let title = gettext("Select a conversation");
                self.imp().message_title.set_title(&title);
                self.show_message_placeholder(&title);
                self.render_closed_thread();
            }
        }
        if application.conversation_changed() {
            self.sync_conversations_from_catalog();
        }
        if application.thread_catalog_changed() {
            match self.current_main_view() {
                MainMessageView::Threads => self.populate_threads(),
                MainMessageView::Unreads if !application.conversation_changed() => {
                    self.populate_unreads(self.unread_items());
                }
                _ => {}
            }
        }
    }

    fn set_sidebar_error(&self, error: &str) {
        let imp = self.imp();
        let has_conversations = !imp.workspace.conversations().is_empty();
        *imp.sidebar_error.borrow_mut() = Some(error.to_string());
        if sidebar_error_change_needs_render(has_conversations) {
            self.render_conversations();
        }
    }

    fn sync_conversations_from_catalog(&self) {
        *self.imp().sidebar_error.borrow_mut() = None;
        self.render_conversations();
        if self.current_main_view() == MainMessageView::Unreads {
            self.populate_unreads(self.unread_items());
        } else {
            self.refresh_current_conversation_title();
        }
        self.imp().workspace_ready.set(true);
        self.activate_pending_notification_target();
        self.activate_pending_slack_uris();
        self.refresh_open_conversation_picker();
    }

    fn populate_user_names(&self, user_names: HashMap<String, String>) {
        if user_names.is_empty() {
            return;
        }

        let changed_user_ids = {
            let imp = self.imp();
            let mut known_user_names = imp.user_names.borrow_mut();
            let known_user_names = Arc::make_mut(&mut known_user_names);
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

        self.user_names_changed(changed_user_ids);
    }

    fn replace_user_directory_identity_maps(&self, users: &[SlackUser]) {
        let maps = user_directory_identity_maps(users);
        let directory_user_ids = discovered_user_ids(users);
        self.imp()
            .pending_user_ids
            .borrow_mut()
            .retain(|user_id| !directory_user_ids.contains(user_id));
        self.replace_user_names(maps.names);
        self.replace_user_full_names(maps.full_names);
        self.replace_user_avatar_urls(maps.avatar_urls);
    }

    fn replace_user_names(&self, replacement: HashMap<String, String>) {
        let changed_user_ids = {
            let mut current = self.imp().user_names.borrow_mut();
            let changed_user_ids = changed_map_keys(current.as_ref(), &replacement);
            *current = Arc::new(replacement);
            changed_user_ids
        };
        self.user_names_changed(changed_user_ids);
    }

    fn user_names_changed(&self, changed_user_ids: Vec<String>) {
        if changed_user_ids.is_empty() {
            return;
        }

        for target in COMPOSER_TARGETS {
            self.refresh_composer_mention_names(target);
        }
        let should_render_sidebar = {
            let imp = self.imp();
            let conversations = imp.workspace.conversations().conversations();
            changed_user_ids
                .iter()
                .any(|user_id| sidebar_user_name_update_needs_render(&conversations, user_id))
        };
        if should_render_sidebar {
            self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
        }
        for user_id in &changed_user_ids {
            self.patch_user_on_timelines(user_id);
        }
        self.update_huddle_surface();
        self.queue_ui_invalidations(UiInvalidations::PICKER | UiInvalidations::TITLE);
        self.flush_pending_message_notifications();
    }

    fn populate_user_full_names(&self, names: HashMap<String, String>) {
        if names.is_empty() {
            return;
        }
        let changed = {
            let mut known = self.imp().user_full_names.borrow_mut();
            let known = Arc::make_mut(&mut known);
            let mut changed = false;
            for (user_id, full_name) in names {
                changed |= known.get(&user_id) != Some(&full_name);
                known.insert(user_id, full_name);
            }
            changed
        };
        if changed {
            self.queue_ui_invalidations(
                UiInvalidations::MAIN
                    | UiInvalidations::THREAD
                    | UiInvalidations::SIDEBAR
                    | UiInvalidations::PICKER,
            );
            self.flush_pending_message_notifications();
        }
    }

    fn replace_user_full_names(&self, replacement: HashMap<String, String>) {
        let changed = {
            let mut current = self.imp().user_full_names.borrow_mut();
            let changed = !changed_map_keys(current.as_ref(), &replacement).is_empty();
            *current = Arc::new(replacement);
            changed
        };
        if changed {
            self.queue_ui_invalidations(
                UiInvalidations::MAIN
                    | UiInvalidations::THREAD
                    | UiInvalidations::SIDEBAR
                    | UiInvalidations::PICKER,
            );
            self.flush_pending_message_notifications();
        }
    }

    fn populate_user_avatar_urls(&self, urls: HashMap<String, String>) {
        if urls.is_empty() {
            return;
        }
        let changed = {
            let mut known = self.imp().user_avatar_urls.borrow_mut();
            let known = Arc::make_mut(&mut known);
            urls.into_iter()
                .filter(|(user_id, url)| !user_id.trim().is_empty() && !url.trim().is_empty())
                .fold(false, |changed, (user_id, url)| {
                    (known.insert(user_id, url.clone()).as_ref() != Some(&url)) || changed
                })
        };
        if changed {
            self.queue_ui_invalidations(UiInvalidations::MAIN | UiInvalidations::THREAD);
        }
    }

    fn replace_user_avatar_urls(&self, replacement: HashMap<String, String>) {
        let changed = {
            let mut current = self.imp().user_avatar_urls.borrow_mut();
            let changed = !changed_map_keys(current.as_ref(), &replacement).is_empty();
            *current = Arc::new(replacement);
            changed
        };
        if changed {
            self.queue_ui_invalidations(UiInvalidations::MAIN | UiInvalidations::THREAD);
        }
    }

    fn populate_user_statuses(&self, statuses: HashMap<String, SlackUserStatus>) {
        if statuses.is_empty() {
            return;
        }
        let changed = {
            let mut known = self.imp().user_statuses.borrow_mut();
            let known = Arc::make_mut(&mut known);
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

    fn apply_user_statuses_snapshot(
        &self,
        statuses: HashMap<String, SlackUserStatus>,
        replace_existing: bool,
        preserve_user_ids: &HashSet<String>,
    ) {
        let mut known = self.imp().user_statuses.borrow_mut();
        let changed = apply_user_status_snapshot(
            Arc::make_mut(&mut known),
            statuses,
            replace_existing,
            preserve_user_ids,
        );
        drop(known);
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
            let known_names = Arc::make_mut(&mut known_names);
            let known_members = Arc::make_mut(&mut known_members);
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
        let conversations = imp.workspace.conversations().conversations();
        let user_names = imp.user_names.borrow().clone();
        let user_search_aliases = imp.user_search_aliases.borrow();
        let selected_channel = self.visible_channel_id();
        let active_huddle_channel_id = imp
            .huddle_snapshot
            .borrow()
            .huddle
            .as_ref()
            .map(|huddle| huddle.channel_id.clone());
        let model = sidebar::build_sidebar_list(
            &conversations,
            &user_names,
            sidebar::SidebarBuildOptions {
                selected_channel: selected_channel.as_deref(),
                active_huddle_channel_id: active_huddle_channel_id.as_deref(),
                current_user_id: imp.current_user_id.borrow().as_deref(),
                query: imp.sidebar_filter_entry.text().as_str(),
                unread_only: imp.sidebar_unread_filter_button.is_active(),
                show_unreads_section: self.show_unreads_section(),
                show_all: imp.sidebar_all_filter_button.is_active(),
                loading: false,
                has_error: imp.sidebar_error.borrow().is_some(),
                user_search_aliases: Some(&user_search_aliases),
                user_full_names: Some(&imp.user_full_names.borrow()),
                user_statuses: Some(&imp.user_statuses.borrow()),
            },
        );

        self.reconcile_sidebar(
            model.keyed_items_with_collapsed_sections(&imp.collapsed_sidebar_sections.borrow()),
        );
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
            SidebarItemModel::SectionHeader {
                title, collapsed, ..
            } => self.sidebar_section_row(title, *collapsed),
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
        row.set_focusable(true);

        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let weak_window = self.downgrade();
        let row_for_menu = row.clone();
        let channel_id_for_pointer = channel_id.to_string();
        gesture.connect_pressed(move |_, _, x, y| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            window.show_sidebar_context_menu(
                &row_for_menu,
                &channel_id_for_pointer,
                Some(gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)),
            );
        });
        row.add_controller(gesture);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak_window = self.downgrade();
        let row_for_menu = row.clone();
        let channel_id_for_keyboard = channel_id.to_string();
        keys.connect_key_pressed(move |_, key, _, state| {
            if !sidebar_context_menu_key(key, state) {
                return glib::Propagation::Proceed;
            }
            let Some(window) = weak_window.upgrade() else {
                return glib::Propagation::Proceed;
            };
            window.show_sidebar_context_menu(
                &row_for_menu,
                &channel_id_for_keyboard,
                Some(gtk::gdk::Rectangle::new(
                    row_for_menu.width() / 2,
                    row_for_menu.height() / 2,
                    1,
                    1,
                )),
            );
            glib::Propagation::Stop
        });
        row.add_controller(keys);
    }

    fn show_sidebar_context_menu(
        &self,
        row: &gtk::ListBoxRow,
        channel_id: &str,
        pointing_to: Option<gtk::gdk::Rectangle>,
    ) {
        let popover = gtk::Popover::new();
        popover.set_parent(row);
        popover.set_pointing_to(pointing_to.as_ref());
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu.set_margin_top(6);
        menu.set_margin_bottom(6);
        menu.set_margin_start(6);
        menu.set_margin_end(6);

        let conversation = self
            .imp()
            .workspace
            .conversations()
            .get(channel_id)
            .cloned();

        if let Some(action) = conversation
            .as_ref()
            .and_then(sidebar_conversation_star_action)
        {
            let star_button = gtk::Button::with_label(&gettext(action.label()));
            star_button.add_css_class("flat");
            let channel_id = channel_id.to_string();
            let weak_window = self.downgrade();
            let popover_for_star = popover.clone();
            star_button.connect_clicked(move |_| {
                popover_for_star.popdown();
                let Some(window) = weak_window.upgrade() else {
                    return;
                };
                window.set_status(&gettext(if action.starred {
                    "Starring conversation..."
                } else {
                    "Unstarring conversation..."
                }));
                window.send_command(RuntimeCommand::SetConversationStarred {
                    channel_id: channel_id.clone(),
                    starred: action.starred,
                });
            });
            menu.append(&star_button);
        }

        if let Some(action) = conversation
            .as_ref()
            .and_then(sidebar_conversation_profile_action)
        {
            let profile_button = gtk::Button::with_label(&gettext(action.label()));
            profile_button.add_css_class("flat");
            let weak_window = self.downgrade();
            let popover_for_profile = popover.clone();
            profile_button.connect_clicked(move |_| {
                popover_for_profile.popdown();
                if let Some(window) = weak_window.upgrade() {
                    window.show_user_profile(&action.user_id);
                }
            });
            menu.append(&profile_button);
        }

        let mark_read_button = gtk::Button::with_label(&gettext("Mark as read"));
        mark_read_button.add_css_class("flat");
        let mark_read_channel_id = channel_id.to_string();
        let weak_window = self.downgrade();
        let popover_for_mark_read = popover.clone();
        mark_read_button.connect_clicked(move |_| {
            popover_for_mark_read.popdown();
            if let Some(window) = weak_window.upgrade() {
                window.mark_channel_read_through_latest(&mark_read_channel_id);
            }
        });
        menu.append(&mark_read_button);

        if let Some(conversation) = conversation.as_ref() {
            let add_people_button = gtk::Button::with_label(&gettext("Add people"));
            add_people_button.add_css_class("flat");
            let conversation = conversation.clone();
            let weak_window = self.downgrade();
            let popover_for_people = popover.clone();
            add_people_button.connect_clicked(move |_| {
                popover_for_people.popdown();
                if let Some(window) = weak_window.upgrade() {
                    window.show_add_people_picker(&conversation);
                }
            });
            menu.append(&add_people_button);
        }
        if let Some(conversation) = conversation.filter(sidebar_conversation_can_leave) {
            let leave_button = gtk::Button::with_label(&gettext("Leave channel"));
            leave_button.add_css_class("flat");
            leave_button.add_css_class("destructive-action");
            let weak_window = self.downgrade();
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

    fn mark_channel_read_through_latest(&self, channel_id: &str) {
        let latest = SlackMessage::latest_ts(
            self.imp()
                .workspace
                .view
                .borrow()
                .channel_messages(channel_id)
                .iter(),
        )
        .or_else(|| {
            self.imp()
                .workspace
                .conversations()
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

    fn sidebar_section_row(&self, title: &str, collapsed: bool) -> gtk::ListBoxRow {
        let header_row = gtk::ListBoxRow::new();
        header_row.set_selectable(false);
        header_row.set_activatable(true);
        header_row.set_focusable(true);
        header_row.update_property(&[gtk::accessible::Property::Label(
            &sidebar_section_accessible_label(title, collapsed),
        )]);

        let header = gtk::Label::new(Some(title));
        header.set_xalign(0.0);
        header.set_hexpand(true);
        header.set_margin_top(12);
        header.set_margin_bottom(3);
        header.set_margin_end(9);
        header.add_css_class("caption");
        header.add_css_class("heading");

        let disclosure = gtk::Image::from_icon_name(if collapsed {
            "pan-end-symbolic"
        } else {
            "pan-down-symbolic"
        });
        disclosure.set_margin_start(9);
        disclosure.set_accessible_role(gtk::AccessibleRole::Presentation);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        content.append(&disclosure);
        content.append(&header);
        header_row.set_child(Some(&content));
        header_row
    }

    fn reconcile_sidebar(&self, next_items: Vec<KeyedSidebarItem>) {
        let imp = self.imp();
        let initialize = imp.sidebar_projection.borrow().items().is_empty()
            && imp.sidebar_rows.borrow().is_empty();
        let change = {
            let mut projection = imp.sidebar_projection.borrow_mut();
            if initialize {
                projection.reset(next_items.clone())
            } else {
                projection.reconcile(next_items.clone())
            }
        };
        let change = match change {
            Ok(change) => change,
            Err(error) => {
                let (reason, count, first_position, duplicate_position) = match error {
                    SidebarProjectionError::TooManyItems { count } => {
                        ("too_many_items", count, None, None)
                    }
                    SidebarProjectionError::DuplicateKey {
                        first_position,
                        duplicate_position,
                        ..
                    } => (
                        "duplicate_key",
                        next_items.len(),
                        Some(first_position),
                        Some(duplicate_position),
                    ),
                };
                tracing::error!(
                    target: "conduit::sidebar",
                    parent: None,
                    event = "sidebar_projection_rejected",
                    reason,
                    count,
                    first_position,
                    duplicate_position,
                );
                return;
            }
        };
        let next_keys = next_items
            .iter()
            .map(|item| &item.key)
            .collect::<HashSet<_>>();

        {
            let mut rows = imp.sidebar_rows.borrow_mut();
            for operation in &change.operations {
                match operation {
                    SidebarProjectionOperation::Reset { items } => {
                        self.clear_list(&imp.conversation_list);
                        rows.clear();
                        for item in items {
                            let row = self.sidebar_item_row(item);
                            imp.conversation_list.append(&row);
                            rows.insert(item.key.clone(), row);
                        }
                    }
                    SidebarProjectionOperation::Splice {
                        position,
                        removed,
                        inserted,
                    } => {
                        for key in removed {
                            if let Some(row) = rows.get(key).cloned() {
                                imp.conversation_list.remove(&row);
                            }
                            if !next_keys.contains(key) {
                                rows.remove(key);
                            }
                        }
                        for (offset, item) in inserted.iter().enumerate() {
                            let row = rows
                                .entry(item.key.clone())
                                .or_insert_with(|| self.sidebar_item_row(item))
                                .clone();
                            let position = position.saturating_add(offset as u32);
                            let position = i32::try_from(position)
                                .expect("GtkListBox sidebar position should fit i32");
                            imp.conversation_list.insert(&row, position);
                        }
                    }
                    SidebarProjectionOperation::Update { key, model, .. } => {
                        let Some(existing) = rows.get(key) else {
                            debug_assert!(false, "updated sidebar key should have a row");
                            continue;
                        };
                        self.update_sidebar_row(existing, key, model);
                    }
                }
            }
        }

        imp.sidebar_row_actions.borrow_mut().clear();
        imp.sidebar_section_actions.borrow_mut().clear();
        let rows = imp.sidebar_rows.borrow();
        for item in &next_items {
            let Some(row) = rows.get(&item.key) else {
                continue;
            };
            match &item.model {
                SidebarItemModel::SectionHeader { kind, .. } => {
                    imp.sidebar_section_actions
                        .borrow_mut()
                        .insert(row.index(), *kind);
                }
                SidebarItemModel::Conversation(model) => {
                    self.register_sidebar_row_action(row.index(), model);
                }
                SidebarItemModel::Placeholder(_) => {}
            }
        }
        let selected = change.selected_key.as_ref().and_then(|key| rows.get(key));
        debug_assert_eq!(
            selected.map(|row| row.index() as u32),
            change.selected_position
        );
        imp.conversation_list.select_row(selected);
        drop(rows);

        let projection = imp.sidebar_projection.borrow();
        debug_assert_eq!(projection.items(), next_items);
        debug_assert_eq!(
            change.selected_position,
            change
                .selected_key
                .as_ref()
                .and_then(|key| projection.position(key))
        );
        debug_assert_eq!(projection.counters().current_items, next_items.len() as u64);
    }

    fn update_sidebar_row(
        &self,
        existing: &gtk::ListBoxRow,
        key: &SidebarItemKey,
        model: &SidebarItemModel,
    ) {
        let replacement = self.sidebar_item_row(&KeyedSidebarItem {
            key: key.clone(),
            model: model.clone(),
        });
        existing.set_selectable(replacement.is_selectable());
        existing.set_activatable(replacement.is_activatable());
        existing.set_focusable(replacement.is_focusable());
        existing.set_tooltip_text(replacement.tooltip_text().as_deref());
        match model {
            SidebarItemModel::SectionHeader {
                title, collapsed, ..
            } => {
                existing.update_property(&[gtk::accessible::Property::Label(
                    &sidebar_section_accessible_label(title, *collapsed),
                )]);
            }
            SidebarItemModel::Conversation(model) => {
                existing.update_property(&[gtk::accessible::Property::Label(
                    &model.accessible_label(),
                )]);
            }
            SidebarItemModel::Placeholder(_) => {}
        }
        if let Some(child) = replacement.child() {
            replacement.set_child(None::<&gtk::Widget>);
            existing.set_child(Some(&child));
        }
    }

    fn register_sidebar_row_action(&self, row_index: i32, model: &SidebarRowModel) {
        self.imp()
            .sidebar_row_actions
            .borrow_mut()
            .insert(row_index, SidebarRowAction::from_model(model));
    }

    fn activate_sidebar_row(&self, row_index: i32) {
        if let Some(section) = self
            .imp()
            .sidebar_section_actions
            .borrow()
            .get(&row_index)
            .copied()
        {
            self.toggle_sidebar_section(section);
            return;
        }
        if let Ok(position) = u32::try_from(row_index) {
            self.imp()
                .sidebar_projection
                .borrow_mut()
                .prefer_selection_at(position);
        }
        let action =
            sidebar_row_action_for_index(&self.imp().sidebar_row_actions.borrow(), row_index);

        if let Some(action) = action {
            let title = self.conversation_title(&action.channel_id);
            self.select_conversation(&action.channel_id, &title);
        }
    }

    fn toggle_sidebar_section(&self, section: SidebarSectionKind) {
        let mut collapsed = self.imp().collapsed_sidebar_sections.borrow_mut();
        toggle_sidebar_section_state(&mut collapsed, section);
        drop(collapsed);
        self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
    }

    fn show_conversation_switcher(&self) {
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
        self.send_command(RuntimeCommand::DiscoverChannels);
        self.request_user_directory();
    }

    fn request_user_directory(&self) {
        if self.imp().user_directory_request_in_flight.replace(true) {
            return;
        }
        self.imp().user_directory_failed.set(false);
        self.refresh_open_people_picker();
        self.send_command(RuntimeCommand::LoadUserDirectory);
    }

    fn show_change_status_dialog(&self) {
        if let Some(state) = self.imp().status_dialog.borrow().as_ref() {
            state.dialog.present(Some(self));
            return;
        }
        if self.imp().pending_status_update.borrow().is_some() {
            self.set_status(&gettext("A status update is already in progress"));
            return;
        }
        let Some(user_id) = self.imp().current_user_id.borrow().clone() else {
            self.set_status(&gettext("Your Slack profile is not available yet"));
            return;
        };
        let now = current_unix_seconds();
        let status = self
            .imp()
            .user_statuses
            .borrow()
            .get(&user_id)
            .filter(|status| status.active_at(now))
            .cloned()
            .unwrap_or_default();
        self.present_change_status_dialog(status, None, false);
    }

    fn present_change_status_dialog(
        &self,
        status: SlackUserStatus,
        error: Option<&str>,
        clearing_retry: bool,
    ) {
        if let Some(state) = self.imp().status_dialog.borrow().as_ref() {
            state.dialog.present(Some(self));
            return;
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_hexpand(true);
        if let Some(error) = error.filter(|error| !error.trim().is_empty()) {
            let error_label = gtk::Label::new(Some(error));
            error_label.set_wrap(true);
            error_label.set_xalign(0.0);
            error_label.add_css_class("error");
            error_label.set_accessible_role(gtk::AccessibleRole::Alert);
            error_label.update_property(&[gtk::accessible::Property::Description(error)]);
            content.append(&error_label);
        }

        let group = adw::PreferencesGroup::new();
        let status_entry = adw::EntryRow::builder()
            .title(gettext("Status"))
            .activates_default(true)
            .build();
        status_entry.set_text(status.text.trim());
        status_entry.set_tooltip_text(Some(&gettext(
            "Enter up to 100 characters for your Slack status",
        )));
        group.add(&status_entry);
        content.append(&group);

        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Set a status"))
            .body(gettext("Choose what teammates see beside your name."))
            .extra_child(&content)
            .default_response("save")
            .close_response("cancel")
            .build();

        let weak_dialog = dialog.downgrade();
        let weak_status_entry = status_entry.downgrade();
        let emoji_picker = StatusEmojiPicker::new(
            &self.imp().custom_emojis.borrow(),
            status.emoji_name(),
            move |selected_emoji| {
                let (Some(dialog), Some(status_entry)) =
                    (weak_dialog.upgrade(), weak_status_entry.upgrade())
                else {
                    return;
                };
                update_status_dialog_save_response(&dialog, &status_entry, selected_emoji);
            },
        );
        group.add(&emoji_picker.row);

        let now = current_unix_seconds();
        let (expiration_labels, expiration_choices, expiration_selected) =
            status_expiration_options(status.expiration, now);
        let expiration_label_refs = expiration_labels
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let expiration_model = gtk::StringList::new(&expiration_label_refs);
        let expiration_row = adw::ComboRow::builder()
            .title(gettext("Clear after"))
            .model(&expiration_model)
            .selected(expiration_selected)
            .build();
        group.add(&expiration_row);

        dialog.add_response("cancel", &gettext("Cancel"));
        if status_dialog_clear_available(&status, now, clearing_retry) {
            dialog.add_response("clear", &gettext("Clear status"));
        }
        dialog.add_response("save", &gettext("Save"));
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        update_status_dialog_save_response(&dialog, &status_entry, &emoji_picker.selected_name());

        {
            let weak_dialog = dialog.downgrade();
            let selected_name = emoji_picker.selected_name_state();
            status_entry.connect_changed(move |entry| {
                enforce_status_text_limit(entry);
                let Some(dialog) = weak_dialog.upgrade() else {
                    return;
                };
                update_status_dialog_save_response(&dialog, entry, &selected_name.borrow());
            });
        }

        let selected_emoji = emoji_picker.selected_name_state();
        let expiration_choice_count = expiration_choices.len();
        let expiration_choices = Rc::new(expiration_choices);
        let weak_status_entry = status_entry.downgrade();
        let weak_expiration_row = expiration_row.downgrade();
        let weak_window = self.downgrade();
        let dialog_draft = status.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            window.imp().status_dialog.borrow_mut().take();
            let pending = match response {
                "clear" => Some(PendingStatusUpdate {
                    requested: SlackUserStatus::default(),
                    dialog_draft: dialog_draft.clone(),
                    clearing: true,
                }),
                "save" => {
                    let (Some(status_entry), Some(expiration_row)) =
                        (weak_status_entry.upgrade(), weak_expiration_row.upgrade())
                    else {
                        return;
                    };
                    let emoji = selected_emoji.borrow().clone();
                    let choice = expiration_choices
                        .get(expiration_row.selected() as usize)
                        .copied()
                        .unwrap_or(StatusExpirationChoice::Never);
                    let now = current_unix_seconds();
                    let (end_today, end_week) = status_expiration_boundaries(now);
                    let status = status_from_dialog_input(
                        status_entry.text().as_str(),
                        &emoji,
                        choice,
                        now,
                        end_today,
                        end_week,
                    );
                    Some(PendingStatusUpdate {
                        requested: status.clone(),
                        dialog_draft: status,
                        clearing: false,
                    })
                }
                _ => None,
            };
            if let Some(pending) = pending {
                window.submit_current_user_status(pending);
            }
        });

        self.imp()
            .status_dialog
            .borrow_mut()
            .replace(StatusDialogState {
                dialog: dialog.clone(),
                status_entry: status_entry.clone(),
                emoji_picker,
                expiration_choice_count,
            });
        dialog.present(Some(self));
        status_entry.grab_focus();
        if let Some(state) = self.imp().status_dialog.borrow().as_ref() {
            write_status_dialog_test_state(self, state);
        }
        if std::env::var_os("CONDUIT_TEST_STATUS_OPEN_EMOJI").is_some() {
            let weak_window = self.downgrade();
            glib::idle_add_local_once(move || {
                let Some(window) = weak_window.upgrade() else {
                    return;
                };
                if let Some(state) = window.imp().status_dialog.borrow().as_ref() {
                    state.emoji_picker.popover.popup();
                    state.emoji_picker.search.grab_focus();
                    write_status_dialog_test_state(&window, state);
                };
            });
        }
        if std::env::var_os("CONDUIT_TEST_STATUS_REOPEN_EMOJI").is_some() {
            let reopened = Rc::new(Cell::new(false));
            if let Some(state) = self.imp().status_dialog.borrow().as_ref() {
                let weak_window = self.downgrade();
                let reopened = reopened.clone();
                state.emoji_picker.popover.connect_closed(move |_| {
                    if reopened.replace(true) {
                        return;
                    }
                    let weak_window = weak_window.clone();
                    glib::idle_add_local_once(move || {
                        let Some(window) = weak_window.upgrade() else {
                            return;
                        };
                        if let Some(state) = window.imp().status_dialog.borrow().as_ref() {
                            state.emoji_picker.popover.popup();
                            state.emoji_picker.search.grab_focus();
                            write_status_dialog_test_state(&window, state);
                        };
                    });
                });
            }
            let weak_window = self.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                if let Some(window) = weak_window.upgrade() {
                    if let Some(state) = window.imp().status_dialog.borrow().as_ref() {
                        state.emoji_picker.popover.popdown();
                    };
                }
            });
        }
    }

    fn submit_current_user_status(&self, pending: PendingStatusUpdate) {
        if self.imp().current_user_id.borrow().is_none() {
            self.set_status(&gettext("Your Slack profile is not available yet"));
            return;
        }
        let status = pending.requested.clone();
        self.imp()
            .pending_status_update
            .borrow_mut()
            .replace(pending);
        self.set_status(&gettext("Updating status..."));
        self.send_command(RuntimeCommand::SetCurrentUserStatus { status });
    }

    fn show_new_message_picker(&self) {
        self.show_people_picker(
            "New message",
            "Start conversation",
            &[],
            |window, user_ids| {
                if user_ids.len() == 1 {
                    window.send_command(RuntimeCommand::OpenDirectMessage {
                        user_id: user_ids[0].clone(),
                    });
                } else {
                    window.send_command(RuntimeCommand::OpenGroupDirectMessage { user_ids });
                }
            },
        );
    }

    fn show_add_people_picker(&self, conversation: &SlackConversation) {
        let excluded = conversation.display_user_ids();
        let conversation = conversation.clone();
        self.show_people_picker(
            "Add people",
            "Add",
            &excluded,
            move |window, mut user_ids| {
                if conversation.is_im.unwrap_or(false) || conversation.is_mpim.unwrap_or(false) {
                    user_ids.extend(conversation.display_user_ids());
                    user_ids.sort();
                    user_ids.dedup();
                    window.send_command(RuntimeCommand::OpenGroupDirectMessage { user_ids });
                } else {
                    window.send_command(RuntimeCommand::InviteToChannel {
                        channel_id: conversation.id.clone(),
                        user_ids,
                    });
                }
            },
        );
    }

    fn show_people_picker<F>(
        &self,
        title: &str,
        confirm_label: &str,
        excluded_user_ids: &[String],
        on_submit: F,
    ) where
        F: Fn(&Self, Vec<String>) + 'static,
    {
        let people_picker = self.imp().people_picker_view.borrow_mut().take();
        if let Some(view) = people_picker {
            view.dialog.close();
        }

        let dialog = gtk::Window::builder()
            .title(title)
            .transient_for(self)
            .modal(true)
            .default_width(480)
            .default_height(560)
            .build();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.set_margin_top(12);
        container.set_margin_bottom(12);
        container.set_margin_start(12);
        container.set_margin_end(12);
        dialog.set_child(Some(&container));
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some(&gettext("Search people")));
        container.append(&search);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vexpand(true);
        scroller.set_child(Some(&list));
        container.append(&scroller);
        let confirm = gtk::Button::with_label(confirm_label);
        confirm.add_css_class("suggested-action");
        confirm.set_sensitive(false);
        container.append(&confirm);
        let rows = Rc::new(RefCell::new(Vec::<PeoplePickerRow>::new()));
        let rows_for_search = rows.clone();
        search.connect_search_changed(move |search| {
            let query = search.text().trim().to_lowercase();
            for row in rows_for_search.borrow().iter() {
                row.row
                    .set_visible(query.is_empty() || row.search_name.contains(&query));
            }
        });
        let weak_window = self.downgrade();
        let dialog_for_submit = dialog.clone();
        let on_submit: PeoplePickerSubmit = Rc::new(on_submit);
        let on_submit_for_click = on_submit.clone();
        let rows_for_submit = rows.clone();
        confirm.connect_clicked(move |_| {
            let user_ids = rows_for_submit
                .borrow()
                .iter()
                .filter(|row| row.check.is_active())
                .map(|row| row.user_id.clone())
                .collect::<Vec<_>>();
            if let Some(window) = weak_window.upgrade() {
                on_submit_for_click(&window, user_ids);
                dialog_for_submit.close();
            }
        });

        *self.imp().people_picker_view.borrow_mut() = Some(PeoplePickerView {
            dialog: dialog.clone(),
            search: search.clone(),
            list: list.clone(),
            confirm,
            rows,
            excluded_user_ids: excluded_user_ids.iter().cloned().collect(),
        });

        let weak_window = self.downgrade();
        let list_for_close = list;
        dialog.connect_close_request(move |_| {
            if let Some(window) = weak_window.upgrade() {
                let mut active = window.imp().people_picker_view.borrow_mut();
                if active
                    .as_ref()
                    .is_some_and(|view| view.list == list_for_close)
                {
                    active.take();
                }
            }
            glib::Propagation::Proceed
        });

        self.refresh_open_people_picker();
        dialog.present();
        search.grab_focus();
        self.request_user_directory();
    }

    fn refresh_open_people_picker(&self) {
        let Some(view) = self.imp().people_picker_view.borrow().clone() else {
            return;
        };
        let selected_user_ids = view
            .rows
            .borrow()
            .iter()
            .filter(|row| row.check.is_active())
            .map(|row| row.user_id.clone())
            .collect::<HashSet<_>>();
        let (people, directory_state) = {
            let imp = self.imp();
            let current_user_id = imp.current_user_id.borrow().clone();
            let users = imp.discovered_users.borrow();
            (
                people_picker_candidates(
                    &users,
                    current_user_id.as_deref(),
                    &view.excluded_user_ids,
                ),
                people_picker_directory_state(
                    imp.user_directory_loaded.get(),
                    imp.user_directory_failed.get(),
                ),
            )
        };

        self.clear_list(&view.list);
        view.rows.borrow_mut().clear();
        view.confirm.set_sensitive(false);
        if people.is_empty() {
            if directory_state == PeoplePickerDirectoryState::Failed {
                self.append_people_picker_load_failure(&view.list);
            } else {
                self.append_placeholder(
                    &view.list,
                    &gettext(people_picker_empty_message(directory_state)),
                );
            }
            return;
        }

        for person in people {
            let check = gtk::CheckButton::with_label(&person.name);
            check.set_margin_top(6);
            check.set_margin_bottom(6);
            check.set_margin_start(9);
            check.set_margin_end(9);
            check.set_active(selected_user_ids.contains(&person.user_id));
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&check));
            view.list.append(&row);
            view.rows.borrow_mut().push(PeoplePickerRow {
                user_id: person.user_id,
                search_name: person.name.to_lowercase(),
                check: check.clone(),
                row,
            });

            let rows = Rc::downgrade(&view.rows);
            let confirm = view.confirm.clone();
            check.connect_toggled(move |_| {
                let selected = rows
                    .upgrade()
                    .is_some_and(|rows| rows.borrow().iter().any(|row| row.check.is_active()));
                confirm.set_sensitive(selected);
            });
        }

        let query = view.search.text().trim().to_lowercase();
        for row in view.rows.borrow().iter() {
            row.row
                .set_visible(query.is_empty() || row.search_name.contains(&query));
        }
        view.confirm
            .set_sensitive(view.rows.borrow().iter().any(|row| row.check.is_active()));
    }

    fn append_people_picker_load_failure(&self, list: &gtk::ListBox) {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_bottom(12);
        content.append(
            &self.placeholder_label(&gettext(people_picker_empty_message(
                PeoplePickerDirectoryState::Failed,
            ))),
        );

        let retry = gtk::Button::with_label(&gettext("Retry"));
        retry.set_halign(gtk::Align::Start);
        retry.set_margin_start(12);
        retry.add_css_class("suggested-action");
        let weak_window = self.downgrade();
        retry.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.request_user_directory();
            }
        });
        content.append(&retry);

        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        row.set_child(Some(&content));
        list.append(&row);
    }

    fn show_new_channel_dialog(&self) {
        let dialog = gtk::Window::builder()
            .title(gettext("New channel"))
            .transient_for(self)
            .modal(true)
            .default_width(440)
            .build();
        let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
        container.set_margin_top(18);
        container.set_margin_bottom(18);
        container.set_margin_start(18);
        container.set_margin_end(18);
        dialog.set_child(Some(&container));
        let name = gtk::Entry::new();
        name.set_placeholder_text(Some(&gettext("channel-name")));
        container.append(&name);
        let private = gtk::CheckButton::with_label(&gettext("Private channel"));
        container.append(&private);
        let create = gtk::Button::with_label(&gettext("Create channel"));
        create.add_css_class("suggested-action");
        create.set_sensitive(false);
        container.append(&create);
        let create_for_name = create.clone();
        name.connect_changed(move |name| {
            create_for_name.set_sensitive(valid_channel_name(name.text().as_str()));
        });
        let weak_window = self.downgrade();
        let dialog_for_create = dialog.clone();
        let name_for_create = name.clone();
        create.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.send_command(RuntimeCommand::CreateChannel {
                    name: name_for_create.text().trim().to_string(),
                    is_private: private.is_active(),
                });
                dialog_for_create.close();
            }
        });
        dialog.present();
        name.grab_focus();
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
        if !include_discovery && self.imp().workspace.conversations().is_empty() {
            self.set_status(&gettext("No conversations loaded"));
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
        self.append_placeholder(&list, &gettext("Loading conversations…"));

        *self.imp().conversation_picker_view.borrow_mut() = Some(ConversationPickerView {
            list: list.clone(),
            search: search.clone(),
            actions: actions.clone(),
            include_discovery,
        });

        let weak_window = self.downgrade();
        search.connect_search_changed(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.cancel_conversation_picker_population();
                window.schedule_picker_filter();
            }
        });

        let weak_window = self.downgrade();
        let list_for_close = list.clone();
        dialog.connect_close_request(move |_| {
            if let Some(window) = weak_window.upgrade() {
                let closed_active_picker = {
                    let mut active = window.imp().conversation_picker_view.borrow_mut();
                    if active
                        .as_ref()
                        .is_some_and(|view| view.list == list_for_close)
                    {
                        active.take();
                        true
                    } else {
                        false
                    }
                };
                if closed_active_picker {
                    window.cancel_conversation_picker_population();
                    let generation = window
                        .imp()
                        .picker_filter_generation
                        .get()
                        .saturating_add(1);
                    window.imp().picker_filter_generation.set(generation);
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
        let weak_window = self.downgrade();
        let list_for_initial_population = list.clone();
        glib::idle_add_local_once(move || {
            if let Some(window) = weak_window.upgrade() {
                let picker_is_active = window
                    .imp()
                    .conversation_picker_view
                    .borrow()
                    .as_ref()
                    .is_some_and(|view| view.list == list_for_initial_population);
                if picker_is_active {
                    window.refresh_open_conversation_picker();
                }
            }
        });
    }

    fn refresh_open_conversation_picker(&self) {
        let Some(view) = self.imp().conversation_picker_view.borrow().clone() else {
            return;
        };
        let query = view.search.text();
        let sections = {
            let imp = self.imp();
            let conversations = imp.workspace.conversations().conversations();
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
        let generation = self.cancel_conversation_picker_population();
        self.clear_list(list);
        actions.borrow_mut().clear();
        let mut population = ConversationPickerPopulation::new(
            generation,
            conversation_picker_population_entries(sections),
        );
        if let Some(batch) = population.next_batch(generation) {
            self.append_conversation_picker_batch(list, actions, batch);
        }
        if population.is_empty() {
            return;
        }

        let weak_window = self.downgrade();
        let list = list.clone();
        let actions = actions.clone();
        glib::idle_add_local(move || {
            let Some(window) = weak_window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !window.conversation_picker_population_is_active(&list, generation) {
                return glib::ControlFlow::Break;
            }
            let Some(batch) = population.next_batch(generation) else {
                return glib::ControlFlow::Break;
            };
            window.append_conversation_picker_batch(&list, &actions, batch);
            if population.is_empty() {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn cancel_conversation_picker_population(&self) -> u64 {
        let generation = self
            .imp()
            .picker_population_generation
            .get()
            .saturating_add(1);
        self.imp().picker_population_generation.set(generation);
        generation
    }

    fn conversation_picker_population_is_active(
        &self,
        list: &gtk::ListBox,
        generation: u64,
    ) -> bool {
        self.imp().picker_population_generation.get() == generation
            && self
                .imp()
                .conversation_picker_view
                .borrow()
                .as_ref()
                .is_some_and(|view| view.list == *list)
    }

    fn append_conversation_picker_batch(
        &self,
        list: &gtk::ListBox,
        actions: &Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
        batch: Vec<ConversationPickerListEntry>,
    ) {
        for entry in batch {
            match entry {
                ConversationPickerListEntry::Header(title) => {
                    self.append_picker_section_header(list, &title)
                }
                ConversationPickerListEntry::Item(item) => {
                    self.append_conversation_picker_row(list, actions, &item)
                }
                ConversationPickerListEntry::Placeholder(text) => {
                    self.append_placeholder(list, &text)
                }
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

    fn refresh_workspace_title_status(&self) {
        let imp = self.imp();
        let current_user_id = imp.current_user_id.borrow().clone();
        let workspace_name = imp.workspace_name.borrow().clone();
        let title = current_user_header_title(
            current_user_id.as_deref(),
            &imp.user_names.borrow(),
            workspace_name.as_deref(),
        );
        imp.workspace_title_label.set_title(&title);

        let presentation = current_user_id
            .as_deref()
            .and_then(|user_id| imp.user_statuses.borrow().get(user_id).cloned())
            .and_then(|status| {
                user_status_presentation(
                    &status,
                    &imp.custom_emojis.borrow(),
                    current_unix_seconds(),
                )
            });
        if let Some(presentation) = presentation {
            imp.workspace_title_label
                .set_subtitle(&presentation.subtitle);
            let tooltip = workspace_name.as_deref().map_or_else(
                || format!("Status: {}", presentation.accessible_text),
                |workspace| {
                    format!(
                        "{}\nStatus: {}",
                        workspace.trim(),
                        presentation.accessible_text
                    )
                },
            );
            imp.workspace_title_label.set_tooltip_text(Some(&tooltip));
            imp.workspace_title_label
                .update_property(&[gtk::accessible::Property::Description(&tooltip)]);
        } else {
            imp.workspace_title_label.set_subtitle("");
            let workspace_name = workspace_name
                .as_deref()
                .map(str::trim)
                .filter(|workspace| !workspace.is_empty());
            imp.workspace_title_label.set_tooltip_text(workspace_name);
            imp.workspace_title_label
                .update_property(&[gtk::accessible::Property::Description(
                    workspace_name.unwrap_or_default(),
                )]);
        }
    }

    fn refresh_conversation_title_status(&self, channel_id: &str) {
        let imp = self.imp();
        let status = imp
            .workspace
            .conversations()
            .get(channel_id)
            .filter(|conversation| conversation.is_im.unwrap_or(false))
            .and_then(|conversation| conversation.user.as_deref().map(str::to_string))
            .and_then(|user_id| imp.user_statuses.borrow().get(&user_id).cloned())
            .and_then(|status| {
                user_status_presentation(
                    &status,
                    &imp.custom_emojis.borrow(),
                    current_unix_seconds(),
                )
            });
        if let Some(status) = status {
            imp.message_title.set_subtitle(&status.subtitle);
            imp.message_title
                .set_tooltip_text(Some(&status.accessible_text));
            imp.message_title
                .update_property(&[gtk::accessible::Property::Description(&format!(
                    "Status: {}",
                    status.accessible_text
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
        self.sync_back_button();
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
        self.update_huddle_surface();
    }

    fn sync_attention_preferences(&self) {
        let Some(settings) = self.imp().settings.borrow().as_ref().cloned() else {
            return;
        };
        let preferences = attention_settings::load(&settings);
        self.flush_pending_message_notifications();
        self.send_command(RuntimeCommand::UpdateAttentionPreferences(preferences));
    }

    fn select_conversation(&self, channel_id: &str, title: &str) {
        self.select_conversation_target(channel_id, title, None);
    }

    fn select_conversation_target(
        &self,
        channel_id: &str,
        title: &str,
        explicit_message_ts: Option<&str>,
    ) {
        self.record_navigation(&MainNavigationTarget::Conversation(channel_id.to_string()));
        self.flush_current_drafts();
        crate::debug::log(
            "ui",
            &format!("select_conversation channel_id={channel_id} title={title}"),
        );
        let imp = self.imp();
        self.withdraw_conversation_notification(channel_id);
        let (has_unread, last_read, unread_count) = imp
            .workspace
            .conversations()
            .get(channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    conversation.last_read_ts().map(ToString::to_string),
                    conversation.unread_activity_count(),
                )
            })
            .unwrap_or_default();
        imp.conversation_opening.borrow_mut().begin(
            channel_id,
            ConversationOpenIntent::choose(
                explicit_message_ts,
                has_unread,
                last_read.as_deref(),
                unread_count,
            ),
        );
        let outcome = imp
            .workspace
            .view
            .borrow_mut()
            .select_conversation(channel_id);
        let current_messages = imp.workspace.view.borrow().snapshot().channel_messages;
        imp.message_title.set_title(title);
        self.refresh_conversation_title_status(channel_id);
        self.restore_channel_draft(channel_id);
        self.set_composer_canonical_text(ComposerTarget::Thread, "");
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
        context.message_control_handles =
            self.replace_message_control_handles(TimelineSurfaceId::Main, channel_id, &messages);
        if !imp.workspace.view.borrow().has_channel_context(channel_id) {
            context.load_more_url = self.channel_load_more_url(channel_id);
        }
        let (has_unread, last_read, unread_count) = imp
            .workspace
            .conversations()
            .get(channel_id)
            .map(|conversation| {
                (
                    conversation.has_unread_activity(),
                    conversation.last_read_ts().map(ToString::to_string),
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
        let active_open_generation = imp
            .conversation_opening
            .borrow()
            .active_generation_for(channel_id);
        let opening_generation = imp
            .conversation_opening
            .borrow()
            .positioning_generation_for(channel_id);
        context.timeline_generation = opening_generation;
        let opening_position = {
            let mut opening = imp.conversation_opening.borrow_mut();
            opening_generation
                .and_then(|generation| opening.resolve_position(generation, channel_id, &messages))
        };
        if opening_position.is_none()
            && imp
                .conversation_opening
                .borrow()
                .active_waits_for_explicit_target(channel_id)
        {
            self.load_message_html(&message_html::placeholder_document(
                &gettext("Messages"),
                &gettext("Loading message context"),
            ));
            return;
        }
        let explicit_focus_ts = imp
            .workspace
            .view
            .borrow_mut()
            .take_channel_focus_for_render(channel_id, &messages);
        let opening_focus_ts = match opening_position {
            Some(ConversationOpenPosition::Latest) => {
                context.timeline_scroll = TimelineScrollBehavior::Bottom;
                None
            }
            Some(ConversationOpenPosition::Message(message_ts)) => {
                context.timeline_scroll = TimelineScrollBehavior::Preserve;
                Some(message_ts)
            }
            None => None,
        };
        let focus_message_ts = opening_focus_ts.or(explicit_focus_ts);
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
        let render_action = active_open_generation.and_then(|generation| {
            imp.conversation_opening
                .borrow_mut()
                .note_render_requested(generation)
        });
        match render_action {
            Some(ConversationOpenRenderAction::HoldReconciliation) => {}
            Some(ConversationOpenRenderAction::Reconcile) => {
                self.apply_timeline_patch(
                    TimelineSurface::Main,
                    message_html::conversation_snapshot_patch(channel_id, &messages, &context),
                    UiInvalidations::MAIN,
                );
            }
            Some(ConversationOpenRenderAction::InitialDocument) | None => {
                let html = generate_html("conversation", || {
                    message_html::conversation_document_with_focus(
                        channel_id,
                        &messages,
                        &context,
                        focus_message_ts.as_deref(),
                    )
                });
                self.load_message_html(&html);
            }
        }
        self.queue_history_render_followups(channel_id, messages);
    }

    fn reconcile_current_conversation_snapshot(&self) {
        let snapshot = self.current_message_snapshot();
        let Some(channel_id) = snapshot.channel_id else {
            return;
        };
        if snapshot.main_view == MainMessageView::Conversation
            && !snapshot.channel_messages.is_empty()
        {
            self.populate_history_with_scroll(
                &channel_id,
                snapshot.channel_messages,
                TimelineScrollBehavior::Preserve,
            );
        }
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
        context.message_control_handles =
            self.replace_message_control_handles(TimelineSurfaceId::Thread, channel_id, &messages);
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
        let observed = self.imp().workspace.threads().inbox_projection(observed);
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

    fn handle_socket_mode_event(
        &self,
        event: SocketModeEvent,
        attention: Option<AttentionDecision>,
    ) {
        match event {
            SocketModeEvent::Message(event) => self.apply_socket_message(*event, attention),
            SocketModeEvent::Reaction(event) => self.apply_socket_reaction(event),
            SocketModeEvent::UserChanged(user) | SocketModeEvent::UserHuddleChanged(user) => {
                let Some(user_id) = user.id.clone() else {
                    return;
                };
                let (directory_changed, directory_user) = {
                    let mut users = self.imp().discovered_users.borrow_mut();
                    let directory_changed =
                        merge_discovered_user_update(&mut users, user.as_ref().clone());
                    let directory_user = users
                        .iter()
                        .find(|candidate| candidate.id.as_deref() == Some(user_id.as_str()))
                        .cloned()
                        .unwrap_or_else(|| user.as_ref().clone());
                    (directory_changed, directory_user)
                };
                if let Some(display_name) = directory_user.display_name() {
                    self.populate_user_names(HashMap::from([(user_id.clone(), display_name)]));
                }
                if let Some(full_name) = directory_user.full_name() {
                    self.populate_user_full_names(HashMap::from([(user_id.clone(), full_name)]));
                }
                if let Some(avatar_url) = directory_user.avatar_url() {
                    self.populate_user_avatar_urls(HashMap::from([(user_id.clone(), avatar_url)]));
                }
                if let Some(profile) = directory_user.profile.as_ref() {
                    let mut statuses = self.imp().user_statuses.borrow_mut();
                    let changed = apply_user_status_profile_update(
                        Arc::make_mut(&mut statuses),
                        &user_id,
                        profile,
                    );
                    drop(statuses);
                    if changed {
                        self.user_statuses_changed(vec![user_id.clone()]);
                    }
                }
                if directory_changed {
                    self.imp()
                        .user_search_aliases
                        .borrow_mut()
                        .insert(user_id, directory_user.search_aliases());
                    self.refresh_open_conversation_picker();
                    self.refresh_open_people_picker();
                    for target in COMPOSER_TARGETS {
                        self.refresh_composer_completion(target);
                    }
                }
            }
            SocketModeEvent::RefreshConversations(_) => {}
        }
    }

    fn apply_socket_message(
        &self,
        event: SocketModeMessageEvent,
        attention: Option<AttentionDecision>,
    ) {
        let channel_id = event.channel_id.clone();
        let message = event.message.clone();
        let current_user_id = self.imp().current_user_id.borrow().clone();
        let was_unread = self
            .imp()
            .workspace
            .conversations()
            .get(&channel_id)
            .is_some_and(SlackConversation::has_unread_activity);
        let conversation_known = self
            .imp()
            .workspace
            .conversations()
            .get(&channel_id)
            .is_some();
        let external_post = event.kind == SocketModeMessageKind::Posted
            && message.user.as_deref() != current_user_id.as_deref();
        if attention.as_ref().is_some_and(|_| external_post) && !conversation_known {
            self.refresh_conversations();
        }
        let became_unread = attention
            .as_ref()
            .is_some_and(|decision| external_post && decision.record_unread && !was_unread);

        let kind = match event.kind {
            SocketModeMessageKind::Posted => RealtimeMessageKind::Posted,
            SocketModeMessageKind::Changed => RealtimeMessageKind::Changed,
            SocketModeMessageKind::Deleted => RealtimeMessageKind::Deleted,
        };
        let outcome = self.apply_timeline_message(&channel_id, &message, kind, became_unread, None);

        if outcome.refresh_unreads {
            self.populate_unreads(self.unread_items());
        } else {
            self.queue_ui_invalidations(UiInvalidations::SIDEBAR);
        }
    }

    fn apply_timeline_message(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        kind: RealtimeMessageKind,
        unread_start: bool,
        arrival: Option<TimelineMessageArrival>,
    ) -> RealtimeMessageOutcome {
        let (channel_dom_kind, thread_dom_kind, channel_contains_message, thread_contains_message) = {
            let state = self.imp().workspace.view.borrow();
            let selected_thread_ts = state.selected_thread_ts();
            let channel_kind =
                realtime_dom_patch_kind(kind, state.channel_messages(channel_id), message);
            let thread_kind = selected_thread_ts
                .filter(|thread_ts| message.belongs_to_thread(thread_ts))
                .map(|_| realtime_dom_patch_kind(kind, state.current_thread_messages(), message))
                .unwrap_or(Some(kind));
            (
                channel_kind,
                thread_kind,
                state.visible_channel_id() == Some(channel_id)
                    && message.belongs_in_channel_timeline(),
                selected_thread_ts.is_some_and(|thread_ts| message.belongs_to_thread(thread_ts)),
            )
        };

        let outcome = self
            .imp()
            .workspace
            .view
            .borrow_mut()
            .apply_realtime_message(channel_id, message.clone(), kind);

        if timeline_patch_needed(outcome.render_channel, arrival, channel_contains_message) {
            if self
                .imp()
                .workspace
                .view
                .borrow()
                .has_channel_context(channel_id)
            {
                self.queue_ui_invalidations(UiInvalidations::MAIN);
            } else if let Some(dom_kind) = channel_dom_kind {
                self.apply_realtime_message_patch(RealtimeMessagePatch {
                    surface: TimelineSurface::Main,
                    channel_id,
                    message,
                    kind: dom_kind,
                    arrival,
                    unread_start,
                    thread_ts: None,
                    fallback: UiInvalidations::MAIN,
                });
            } else {
                self.queue_ui_invalidations(UiInvalidations::MAIN);
            }
        }

        if timeline_patch_needed(outcome.render_thread, arrival, thread_contains_message) {
            if let Some(thread_ts) = self.selected_thread_ts() {
                if self
                    .imp()
                    .workspace
                    .view
                    .borrow()
                    .has_thread_context(channel_id, &thread_ts)
                {
                    self.queue_ui_invalidations(UiInvalidations::THREAD);
                } else if let Some(dom_kind) = thread_dom_kind {
                    self.apply_realtime_message_patch(RealtimeMessagePatch {
                        surface: TimelineSurface::Thread,
                        channel_id,
                        message,
                        kind: dom_kind,
                        arrival,
                        unread_start: false,
                        thread_ts: Some(&thread_ts),
                        fallback: UiInvalidations::THREAD,
                    });
                } else {
                    self.queue_ui_invalidations(UiInvalidations::THREAD);
                }
            }
        }

        self.request_user_names(std::slice::from_ref(message));
        self.request_image_assets(std::iter::once(message));
        outcome
    }

    fn apply_socket_reaction(&self, event: SocketModeReactionEvent) {
        self.apply_reaction_update(ReactionUpdate {
            channel_id: event.channel_id,
            ts: event.ts,
            name: event.name,
            user_id: event.user_id,
            added: event.added,
        });
    }

    fn apply_reaction_update(&self, update: ReactionUpdate) {
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

    fn handle_huddle_event(&self, event: HuddleEvent) {
        match event {
            HuddleEvent::Snapshot(snapshot) => {
                let previous_phase = self.imp().huddle_snapshot.borrow().phase;
                self.request_user_ids(
                    snapshot
                        .participants
                        .iter()
                        .map(|participant| participant.user_id.clone())
                        .collect(),
                );
                *self.imp().huddle_snapshot.borrow_mut() = (*snapshot).clone();

                if snapshot.phase != HuddlePhase::Preflight {
                    if let Some(preflight) = self.imp().huddle_preflight_dialog.borrow_mut().take()
                    {
                        preflight.dialog.force_close();
                    }
                }
                self.sync_huddle_notification(&snapshot);
                self.update_huddle_surface();
                self.queue_ui_invalidations(UiInvalidations::SIDEBAR);

                if snapshot.phase == HuddlePhase::Preflight
                    && previous_phase != HuddlePhase::Preflight
                {
                    self.show_huddle_preflight();
                } else if snapshot.phase == HuddlePhase::Preflight {
                    self.refresh_huddle_preflight_devices();
                }
            }
            HuddleEvent::DevicesAvailable(devices) => {
                *self.imp().huddle_devices.borrow_mut() = devices;
                self.refresh_huddle_preflight_devices();
            }
            HuddleEvent::OpenExternalRequested(huddle) => match external_huddle_url(&huddle) {
                Ok(uri) => {
                    self.open_external_link(&uri);
                    self.set_status(&gettext("Opened huddle in Slack"));
                }
                Err(_) => self.set_status(&gettext("Slack returned an invalid huddle link.")),
            },
        }
    }

    fn update_huddle_surface(&self) {
        let imp = self.imp();
        let snapshot = imp.huddle_snapshot.borrow().clone();
        let presentation = present_huddle(&snapshot, self.visible_channel_id().as_deref());
        imp.huddle_revealer.set_reveal_child(presentation.visible);
        if !presentation.visible {
            record_test_huddle_surface(imp);
            return;
        }

        imp.huddle_title_label
            .set_label(&gettext(presentation.title));
        imp.huddle_detail_label
            .set_label(&self.huddle_detail(&snapshot));
        imp.huddle_primary_button
            .set_visible(presentation.primary_label.is_some());
        if let Some(label) = presentation.primary_label {
            imp.huddle_primary_button.set_label(&gettext(label));
        }
        imp.huddle_external_button
            .set_visible(presentation.show_external);
        imp.huddle_controls_box
            .set_visible(presentation.show_controls);
        imp.huddle_mute_button
            .set_sensitive(presentation.controls_sensitive);
        imp.huddle_camera_button
            .set_sensitive(presentation.controls_sensitive);
        imp.huddle_share_button.set_sensitive(
            presentation.controls_sensitive && !presentation.screen_share_requesting,
        );
        imp.huddle_leave_button.set_visible(presentation.show_leave);
        imp.huddle_dismiss_button
            .set_visible(presentation.show_dismiss);

        let (mute_icon, mute_label) = if presentation.microphone_muted {
            ("microphone-disabled-symbolic", gettext("Unmute microphone"))
        } else {
            (
                "microphone-sensitivity-high-symbolic",
                gettext("Mute microphone"),
            )
        };
        set_huddle_button_state(&imp.huddle_mute_button, mute_icon, &mute_label);

        let (camera_icon, camera_label) = if presentation.camera_enabled {
            ("camera-video-symbolic", gettext("Turn camera off"))
        } else {
            ("camera-video-symbolic", gettext("Turn camera on"))
        };
        set_huddle_button_state(&imp.huddle_camera_button, camera_icon, &camera_label);

        let (share_icon, share_label) = if presentation.screen_share_requesting {
            (
                "video-display-symbolic",
                gettext("Waiting for screen sharing permission"),
            )
        } else if presentation.screen_share_active {
            ("video-display-symbolic", gettext("Stop sharing screen"))
        } else {
            ("video-display-symbolic", gettext("Share screen"))
        };
        set_huddle_button_state(&imp.huddle_share_button, share_icon, &share_label);
        record_test_huddle_surface(imp);
    }

    fn huddle_detail(&self, snapshot: &HuddleSnapshot) -> String {
        if let Some(failure) = snapshot.failure.as_ref() {
            return gettext(&failure.message);
        }
        if let Some(failure) = snapshot.screen_share_failure.as_ref() {
            return gettext(&failure.message);
        }

        let names = self.imp().user_names.borrow();
        let mut participant_names = snapshot
            .participants
            .iter()
            .filter_map(|participant| names.get(&participant.user_id).cloned())
            .collect::<Vec<_>>();
        participant_names.sort();
        participant_names.dedup();
        let participant_text = if participant_names.is_empty() {
            match snapshot.participants.len() {
                0 => gettext("No participant details"),
                1 => gettext("1 participant"),
                count => format!("{count} {}", gettext("participants")),
            }
        } else {
            participant_names
                .into_iter()
                .take(3)
                .collect::<Vec<_>>()
                .join(", ")
        };

        let mut indicators = Vec::new();
        if snapshot.phase == HuddlePhase::Connected {
            indicators.push(if snapshot.controls.microphone_muted {
                gettext("mic muted")
            } else {
                gettext("mic on")
            });
            indicators.push(if snapshot.controls.camera_enabled {
                gettext("camera on")
            } else {
                gettext("camera off")
            });
            if snapshot.screen_share_state == HuddleScreenShareState::Active {
                indicators.push(gettext("sharing screen"));
            }
        }
        if indicators.is_empty() {
            participant_text
        } else {
            format!("{participant_text} · {}", indicators.join(" · "))
        }
    }

    fn activate_huddle_primary_action(&self) {
        let snapshot = self.imp().huddle_snapshot.borrow().clone();
        let presentation = present_huddle(&snapshot, self.visible_channel_id().as_deref());
        let Some(call_id) = snapshot.call_id().map(str::to_string) else {
            return;
        };
        let command = match presentation.primary_action {
            HuddlePrimaryAction::None => return,
            HuddlePrimaryAction::OpenPreflight => HuddleCommand::OpenPreflight { call_id },
            HuddlePrimaryAction::Join => HuddleCommand::Join { call_id },
            HuddlePrimaryAction::OpenExternal => HuddleCommand::OpenExternally { call_id },
        };
        self.send_command(RuntimeCommand::Huddle(command));
    }

    fn open_active_huddle_externally(&self) {
        let Some(call_id) = self
            .imp()
            .huddle_snapshot
            .borrow()
            .call_id()
            .map(str::to_string)
        else {
            return;
        };
        self.send_command(RuntimeCommand::Huddle(HuddleCommand::OpenExternally {
            call_id,
        }));
    }

    fn toggle_huddle_mute(&self) {
        let muted = self
            .imp()
            .huddle_snapshot
            .borrow()
            .controls
            .microphone_muted;
        self.send_command(RuntimeCommand::Huddle(HuddleCommand::SetMuted(!muted)));
    }

    fn toggle_huddle_camera(&self) {
        let enabled = self.imp().huddle_snapshot.borrow().controls.camera_enabled;
        self.send_command(RuntimeCommand::Huddle(HuddleCommand::SetCameraEnabled(
            !enabled,
        )));
    }

    fn toggle_huddle_screen_share(&self) {
        let enabled = self
            .imp()
            .huddle_snapshot
            .borrow()
            .controls
            .screen_share_enabled;
        self.send_command(RuntimeCommand::Huddle(
            HuddleCommand::SetScreenShareEnabled(!enabled),
        ));
    }

    fn dismiss_huddle(&self) {
        self.send_command(RuntimeCommand::Huddle(HuddleCommand::Dismiss));
    }

    fn show_huddle_preflight(&self) {
        if self.imp().huddle_preflight_dialog.borrow().is_some() {
            return;
        }
        let snapshot = self.imp().huddle_snapshot.borrow().clone();
        if snapshot.phase != HuddlePhase::Preflight || snapshot.huddle.is_none() {
            return;
        }

        let microphone = self.huddle_device_picker(HuddleDeviceKind::Microphone);
        let speaker = self.huddle_device_picker(HuddleDeviceKind::Speaker);
        let camera = self.huddle_device_picker(HuddleDeviceKind::Camera);
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 9);
        choices.append(&huddle_device_row(
            &gettext("Microphone"),
            &microphone.dropdown,
        ));
        choices.append(&huddle_device_row(&gettext("Speaker"), &speaker.dropdown));
        choices.append(&huddle_device_row(&gettext("Camera"), &camera.dropdown));
        let privacy = gtk::Label::new(Some(&gettext(
            "Camera and screen sharing stay off until you turn them on.",
        )));
        privacy.set_wrap(true);
        privacy.set_xalign(0.0);
        privacy.add_css_class("caption");
        privacy.add_css_class("dim-label");
        choices.append(&privacy);

        let primary_label = if snapshot.native_join_available {
            gettext("Join")
        } else {
            gettext("Open in Slack")
        };
        let dialog = adw::AlertDialog::builder()
            .heading(gettext("Join Slack huddle"))
            .body(if snapshot.native_join_available {
                gettext("Choose devices before joining. Capture starts only after you join.")
            } else {
                gettext(
                    "Native joining is unavailable for this Slack session. Review the devices, then continue in Slack.",
                )
            })
            .extra_child(&choices)
            .default_response("primary")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("primary", &primary_label);
        dialog.set_response_appearance("primary", adw::ResponseAppearance::Suggested);

        let weak_window = self.downgrade();
        dialog.connect_response(None, move |_, response| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            window.imp().huddle_preflight_dialog.borrow_mut().take();
            if response == "primary" {
                window.activate_huddle_primary_action();
            } else if window.imp().huddle_snapshot.borrow().phase == HuddlePhase::Preflight {
                window.dismiss_huddle();
            }
        });

        self.imp()
            .huddle_preflight_dialog
            .borrow_mut()
            .replace(HuddlePreflightDialog {
                dialog: dialog.clone(),
                microphone,
                speaker,
                camera,
            });
        self.refresh_huddle_preflight_devices();
        dialog.present(Some(self));
    }

    fn huddle_device_picker(&self, kind: HuddleDeviceKind) -> HuddleDevicePicker {
        let dropdown = gtk::DropDown::from_strings(&[&gettext("System default")]);
        dropdown.set_hexpand(true);
        dropdown.set_enable_search(true);
        let ids = Rc::new(RefCell::new(Vec::<String>::new()));
        let updating = Rc::new(Cell::new(false));
        let weak_window = self.downgrade();
        let ids_for_selection = Rc::clone(&ids);
        let updating_for_selection = Rc::clone(&updating);
        dropdown.connect_selected_notify(move |dropdown| {
            if updating_for_selection.get() {
                return;
            }
            let Some(id) = ids_for_selection
                .borrow()
                .get(dropdown.selected() as usize)
                .cloned()
            else {
                return;
            };
            if let Some(window) = weak_window.upgrade() {
                window.send_command(RuntimeCommand::Huddle(HuddleCommand::SelectDevice {
                    kind,
                    id,
                }));
            }
        });
        HuddleDevicePicker {
            dropdown,
            ids,
            updating,
        }
    }

    fn refresh_huddle_preflight_devices(&self) {
        let Some(preflight) = self.imp().huddle_preflight_dialog.borrow().clone() else {
            return;
        };
        let devices = self.imp().huddle_devices.borrow().clone();
        let selection = self.imp().huddle_snapshot.borrow().devices.clone();
        for (picker, kind) in [
            (&preflight.microphone, HuddleDeviceKind::Microphone),
            (&preflight.speaker, HuddleDeviceKind::Speaker),
            (&preflight.camera, HuddleDeviceKind::Camera),
        ] {
            update_huddle_device_picker(picker, kind, &devices, selection.selected(kind));
        }
    }

    fn sync_huddle_notification(&self, snapshot: &HuddleSnapshot) {
        if snapshot.phase == HuddlePhase::Discovered {
            let Some(huddle) = snapshot.huddle.as_ref() else {
                return;
            };
            if self.imp().notified_huddle_call_id.borrow().as_deref()
                == Some(huddle.call_id.as_str())
            {
                return;
            }
            self.withdraw_huddle_notification();
            let Some(workspace_id) = self.imp().workspace_id.borrow().clone() else {
                return;
            };
            let Some(application) = self
                .application()
                .and_then(|application| application.downcast::<crate::ConduitApplication>().ok())
            else {
                return;
            };
            application.send_huddle_notification(
                &workspace_id,
                &huddle.channel_id,
                &huddle.call_id,
            );
            self.imp()
                .notified_huddle_call_id
                .borrow_mut()
                .replace(huddle.call_id.clone());
        } else if matches!(
            snapshot.phase,
            HuddlePhase::Idle
                | HuddlePhase::Preflight
                | HuddlePhase::Joining
                | HuddlePhase::Connected
                | HuddlePhase::ExternallyHandedOff
        ) {
            self.withdraw_huddle_notification();
        }
    }

    fn withdraw_huddle_notification(&self) {
        let Some(call_id) = self.imp().notified_huddle_call_id.borrow_mut().take() else {
            return;
        };
        let Some(workspace_id) = self.imp().workspace_id.borrow().clone() else {
            return;
        };
        let Some(application) = self
            .application()
            .and_then(|application| application.downcast::<crate::ConduitApplication>().ok())
        else {
            return;
        };
        application.withdraw_huddle_notification(&workspace_id, &call_id);
    }

    fn handle_attention_notification_candidate(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        decision: &AttentionDecision,
    ) {
        let remains_relevant = self
            .imp()
            .settings
            .borrow()
            .as_ref()
            .map(attention_settings::load)
            .is_some_and(|preferences| {
                decision.remains_notification_relevant(&message.visible_text(), &preferences)
            });
        if !remains_relevant {
            return;
        }
        let muted = self
            .imp()
            .workspace
            .conversations()
            .get(channel_id)
            .is_some_and(SlackConversation::is_muted_conversation);
        if attention_notification_should_deliver(
            self.is_active(),
            self.visible_channel_id().as_deref(),
            channel_id,
            muted,
        ) {
            self.send_or_defer_message_notification(channel_id, message, decision);
        }
    }

    fn send_or_defer_message_notification(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        decision: &AttentionDecision,
    ) {
        let conversation = self
            .imp()
            .workspace
            .conversations()
            .get(channel_id)
            .cloned();
        let content = message_notification_conversation(
            conversation.as_ref(),
            &self.imp().user_names.borrow(),
            &self.imp().user_full_names.borrow(),
            self.imp().current_user_id.borrow().as_deref(),
        )
        .and_then(|(title, channel_notification)| {
            message_notification_content(
                &title,
                channel_notification,
                message,
                &self.imp().user_names.borrow(),
            )
        });
        if let Some((title, body)) = content {
            self.send_notification(channel_id, &title, &body, message.thread_root_ts());
            return;
        }

        let key = (channel_id.to_string(), message.ts.clone());
        let mut pending = self.imp().pending_message_notifications.borrow_mut();
        if !pending.contains_key(&key) && pending.len() >= MAX_PENDING_MESSAGE_NOTIFICATIONS {
            if let Some(oldest) = pending
                .iter()
                .min_by(|(_, left), (_, right)| left.message.ts.cmp(&right.message.ts))
                .map(|(key, _)| key.clone())
            {
                pending.remove(&oldest);
            }
        }
        pending.insert(
            key,
            PendingMessageNotification {
                channel_id: channel_id.to_string(),
                message: message.clone(),
                decision: decision.clone(),
            },
        );
        drop(pending);
        let mut user_ids = conversation
            .as_ref()
            .into_iter()
            .flat_map(SlackConversation::display_user_ids)
            .chain(rendering::extract_user_ids(message))
            .collect::<Vec<_>>();
        user_ids.sort();
        user_ids.dedup();
        self.request_user_ids(user_ids);
    }

    fn flush_pending_message_notifications(&self) {
        let current_preferences = self
            .imp()
            .settings
            .borrow()
            .as_ref()
            .map(attention_settings::load);
        let user_names = self.imp().user_names.borrow().clone();
        let user_full_names = self.imp().user_full_names.borrow().clone();
        let current_user_id = self.imp().current_user_id.borrow().clone();
        let pending = self
            .imp()
            .pending_message_notifications
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for notification in pending {
            let key = (
                notification.channel_id.clone(),
                notification.message.ts.clone(),
            );
            if !current_preferences.as_ref().is_some_and(|preferences| {
                notification.decision.remains_notification_relevant(
                    &notification.message.visible_text(),
                    preferences,
                )
            }) {
                self.imp()
                    .pending_message_notifications
                    .borrow_mut()
                    .remove(&key);
                continue;
            }
            let conversation = self
                .imp()
                .workspace
                .conversations()
                .get(&notification.channel_id)
                .cloned();
            let muted = conversation
                .as_ref()
                .is_some_and(SlackConversation::is_muted_conversation);
            if !attention_notification_should_deliver(
                self.is_active(),
                self.visible_channel_id().as_deref(),
                &notification.channel_id,
                muted,
            ) {
                self.imp()
                    .pending_message_notifications
                    .borrow_mut()
                    .remove(&key);
                continue;
            }
            let Some((title, channel_notification)) = message_notification_conversation(
                conversation.as_ref(),
                &user_names,
                &user_full_names,
                current_user_id.as_deref(),
            ) else {
                continue;
            };
            let Some((title, body)) = message_notification_content(
                &title,
                channel_notification,
                &notification.message,
                &user_names,
            ) else {
                continue;
            };
            self.imp()
                .pending_message_notifications
                .borrow_mut()
                .remove(&key);
            self.send_notification(
                &notification.channel_id,
                &title,
                &body,
                notification.message.thread_root_ts(),
            );
        }
    }

    fn send_notification(
        &self,
        channel_id: &str,
        title: &str,
        body: &str,
        thread_ts: Option<&str>,
    ) {
        let Some(workspace_id) = self.imp().workspace_id.borrow().clone() else {
            return;
        };
        let Some(application) = self
            .application()
            .and_then(|application| application.downcast::<crate::ConduitApplication>().ok())
        else {
            return;
        };

        application.send_conversation_notification(
            &workspace_id,
            channel_id,
            title,
            body,
            thread_ts,
        );
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
        thread_ts: Option<String>,
    ) -> bool {
        *self.imp().pending_notification_target.borrow_mut() = Some(NotificationTarget {
            workspace_id,
            channel_id,
            thread_ts,
        });
        self.activate_pending_notification_target()
    }

    pub(crate) fn open_slack_uri(&self, uri: SlackUri) -> bool {
        {
            let mut pending = self.imp().pending_slack_uris.borrow_mut();
            if pending.len() == MAX_PENDING_SLACK_URIS {
                pending.pop_front();
            }
            pending.push_back(uri);
        }
        let opened = self.activate_pending_slack_uris();
        self.present();
        opened
    }

    fn activate_pending_slack_uris(&self) -> bool {
        let mut opened = false;
        loop {
            let Some(uri) = self.imp().pending_slack_uris.borrow().front().cloned() else {
                break;
            };
            let current_team_id = self.imp().workspace_team_id.borrow().clone();
            match resolve_slack_uri(
                current_team_id.as_deref(),
                self.imp().workspace_ready.get(),
                &uri,
            ) {
                SlackUriResolution::Wait => break,
                SlackUriResolution::RejectWorkspace => {
                    self.imp().pending_slack_uris.borrow_mut().pop_front();
                    self.set_status(&gettext(
                        "This Slack link belongs to a different workspace.",
                    ));
                }
                SlackUriResolution::Open => {
                    self.imp().pending_slack_uris.borrow_mut().pop_front();
                    self.activate_slack_uri(uri);
                    opened = true;
                }
            }
        }
        opened
    }

    fn activate_slack_uri(&self, uri: SlackUri) {
        match uri.target().clone() {
            SlackUriTarget::Open => self.present(),
            SlackUriTarget::Channel(channel_id) => {
                if channel_id.starts_with('D') {
                    let title = self.conversation_title(&channel_id);
                    self.select_conversation(&channel_id, &title);
                } else {
                    self.open_channel_reference(&channel_id);
                }
            }
            SlackUriTarget::User(user_id) => {
                self.send_command(RuntimeCommand::OpenDirectMessage { user_id });
            }
            SlackUriTarget::File { file_id, action } => {
                self.show_slack_file(&file_id, action == SlackFileAction::Share);
            }
            SlackUriTarget::App { app_id, .. } => {
                if let Some(team_id) = uri.team_id() {
                    self.open_external_link(&slack_app_web_fallback(team_id, &app_id));
                }
            }
        }
    }

    fn activate_pending_notification_target(&self) -> bool {
        let Some(target) = self.imp().pending_notification_target.borrow().clone() else {
            return false;
        };
        let current_workspace_id = self.imp().workspace_id.borrow().clone();
        match notification_target_resolution(
            current_workspace_id.as_deref(),
            self.imp().workspace_ready.get(),
            &target,
        ) {
            NotificationTargetResolution::Wait => false,
            NotificationTargetResolution::RejectWorkspace => {
                self.imp().pending_notification_target.borrow_mut().take();
                self.set_status(&gettext(
                    "This notification belongs to a different workspace.",
                ));
                false
            }
            NotificationTargetResolution::Open => {
                self.imp().pending_notification_target.borrow_mut().take();
                let conversations = self.imp().workspace.conversations().conversations();
                let opened = match conversation_target_action(&target.channel_id, &conversations) {
                    ConversationTargetAction::SelectConversation(channel_id) => {
                        let title = self.conversation_title(&channel_id);
                        self.select_conversation(&channel_id, &title);
                        if let Some(thread_ts) = target.thread_ts.as_deref() {
                            self.open_thread(&channel_id, thread_ts);
                        }
                        self.visible_channel_id().as_deref() == Some(channel_id.as_str())
                    }
                    ConversationTargetAction::OpenDirectMessage(user_id) => {
                        self.send_command(RuntimeCommand::OpenDirectMessage { user_id });
                        true
                    }
                };
                self.present();
                opened
            }
        }
    }

    fn conversation_title(&self, channel_id: &str) -> String {
        let imp = self.imp();
        let user_names = imp.user_names.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        imp.workspace
            .conversations()
            .get(channel_id)
            .map(|conversation| {
                conversation.display_name_with_users(&user_names, current_user_id.as_deref())
            })
            .unwrap_or_else(|| "Slack".to_string())
    }

    fn open_channel_reference(&self, channel_id: &str) {
        if self
            .imp()
            .workspace
            .conversations()
            .get(channel_id)
            .is_some()
        {
            let title = self.conversation_title(channel_id);
            self.select_conversation(channel_id, &title);
        } else {
            self.send_command(RuntimeCommand::JoinConversation {
                channel_id: channel_id.to_string(),
            });
        }
    }

    fn unread_items(&self) -> Vec<ActivityItem> {
        let imp = self.imp();
        let conversations = imp.workspace.conversations().conversations();
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
            imp.workspace.threads().clone().into_records(),
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
        self.imp()
            .message_control_registry
            .borrow_mut()
            .reset_session();
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

    fn request_user_ids(&self, ids: Vec<String>) {
        let imp = self.imp();
        let known_user_ids = imp.user_names.borrow().keys().cloned().collect();
        let directory_user_ids = discovered_user_ids(&imp.discovered_users.borrow());
        let plan = plan_user_id_lookups(
            ids,
            &known_user_ids,
            &directory_user_ids,
            &imp.pending_user_ids.borrow(),
            imp.user_directory_loaded.get(),
            imp.user_directory_failed.get(),
            imp.user_directory_request_in_flight.get(),
        );
        imp.deferred_user_ids.borrow_mut().extend(plan.deferred_ids);
        if plan.request_directory {
            self.request_user_directory();
        }
        self.send_individual_user_lookups(plan.individual_ids);
    }

    fn resolve_deferred_user_ids(&self) {
        let imp = self.imp();
        let deferred_ids = std::mem::take(&mut *imp.deferred_user_ids.borrow_mut());
        if deferred_ids.is_empty() {
            return;
        }
        let known_user_ids = imp.user_names.borrow().keys().cloned().collect();
        let directory_user_ids = discovered_user_ids(&imp.discovered_users.borrow());
        let plan = plan_user_id_lookups(
            deferred_ids.into_iter().collect(),
            &known_user_ids,
            &directory_user_ids,
            &imp.pending_user_ids.borrow(),
            true,
            false,
            false,
        );
        self.send_individual_user_lookups(plan.individual_ids);
    }

    fn send_individual_user_lookups(&self, ids: Vec<String>) {
        for user_id in ids {
            if self
                .imp()
                .pending_user_ids
                .borrow_mut()
                .insert(user_id.clone())
            {
                self.send_command(RuntimeCommand::LoadUser { user_id });
            }
        }
    }

    fn request_image_assets<'a>(&self, messages: impl IntoIterator<Item = &'a SlackMessage>) {
        let avatar_urls = self.imp().user_avatar_urls.borrow();
        let requests = message_image_asset_requests(messages, &avatar_urls);
        drop(avatar_urls);
        if requests.is_empty() {
            return;
        }

        let known_assets = self.imp().image_assets.borrow();
        let failed_assets = self.imp().failed_image_assets.borrow();
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
        drop(failed_assets);
        drop(known_assets);

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
        let avatar_urls = self.imp().user_avatar_urls.borrow();
        let image_keys = message_image_asset_requests([message], &avatar_urls)
            .into_iter()
            .map(|(key, _)| key)
            .collect::<HashSet<_>>();
        drop(avatar_urls);
        let mut context = self.message_html_context_with_image_keys(thread_ts, Some(&image_keys));
        if let Some(channel_id) = self.visible_channel_id() {
            let surface = if thread_ts.is_some() {
                TimelineSurfaceId::Thread
            } else {
                TimelineSurfaceId::Main
            };
            if let Ok(target) = MessageRef::new(channel_id, message.ts.clone()) {
                if let Some(handle) = self
                    .imp()
                    .message_control_registry
                    .borrow()
                    .active_handle(surface, &target)
                {
                    context.message_control_handles.insert(target, handle);
                }
            }
        }
        context
    }

    fn replace_message_control_handles(
        &self,
        surface: TimelineSurfaceId,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> HashMap<MessageRef, crate::message_handoff::MessageControlHandle> {
        let targets = messages
            .iter()
            .filter_map(|message| MessageRef::new(channel_id, message.ts.clone()).ok())
            .collect::<Vec<_>>();
        self.imp()
            .message_control_registry
            .borrow_mut()
            .replace_surface(surface, targets)
            .unwrap_or_default()
    }

    fn message_html_context_with_image_keys(
        &self,
        thread_ts: Option<&str>,
        image_keys: Option<&HashSet<String>>,
    ) -> MessageHtmlContext {
        let imp = self.imp();
        let user_names = imp.user_names.borrow().clone();
        let current_user_id = imp.current_user_id.borrow().clone();
        let mut conversation_titles = imp
            .discovered_channels
            .borrow()
            .iter()
            .map(|conversation| {
                let title =
                    conversation.display_name_with_users(&user_names, current_user_id.as_deref());
                (conversation.id.clone(), title)
            })
            .collect::<HashMap<_, _>>();
        conversation_titles.extend(
            imp.workspace
                .conversations()
                .conversations()
                .into_iter()
                .map(|conversation| {
                    let title = conversation
                        .display_name_with_users(&user_names, current_user_id.as_deref());
                    (conversation.id, title)
                }),
        );
        let recent_reactions = imp
            .settings
            .borrow()
            .as_ref()
            .map(|settings| settings.strv(config::RECENT_REACTIONS_KEY))
            .map(|names| names.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        MessageHtmlContext {
            user_names,
            user_full_names: imp.user_full_names.borrow().clone(),
            user_avatar_urls: imp.user_avatar_urls.borrow().clone(),
            conversation_titles,
            user_statuses: imp.user_statuses.borrow().clone(),
            user_group_names: imp.user_group_names.borrow().clone(),
            user_group_members: imp.user_group_members.borrow().clone(),
            current_user_id,
            thread_ts: thread_ts.map(ToString::to_string),
            load_more_url: None,
            timeline_scroll: TimelineScrollBehavior::Preserve,
            timeline_generation: None,
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
            message_control_handles: HashMap::new(),
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

fn set_huddle_button_state(button: &gtk::Button, icon_name: &str, label: &str) {
    button.set_icon_name(icon_name);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
}

fn record_test_huddle_surface(window: &imp::ConduitWindow) {
    let Some(path) = std::env::var_os("CONDUIT_TEST_HUDDLE_UI_FILE") else {
        return;
    };
    let primary_label = window
        .huddle_primary_button
        .label()
        .map(|label| label.to_string());
    let _ = std::fs::write(
        path,
        serde_json::json!({
            "visible": window.huddle_revealer.reveals_child(),
            "title": window.huddle_title_label.label().to_string(),
            "detail": window.huddle_detail_label.label().to_string(),
            "primary_visible": window.huddle_primary_button.is_visible(),
            "primary_label": primary_label,
            "external_visible": window.huddle_external_button.is_visible(),
            "controls_visible": window.huddle_controls_box.is_visible(),
            "dismiss_visible": window.huddle_dismiss_button.is_visible(),
            "camera_sensitive": window.huddle_camera_button.is_sensitive(),
            "share_sensitive": window.huddle_share_button.is_sensitive(),
        })
        .to_string(),
    );
}

fn write_status_dialog_test_state(window: &ConduitWindow, state: &StatusDialogState) {
    let Some(path) = std::env::var_os("CONDUIT_TEST_STATUS_UI_FILE") else {
        return;
    };
    let imp = window.imp();
    let _ = std::fs::write(
        path,
        serde_json::json!({
            "dialog_heading": state.dialog.heading().map(|heading| heading.to_string()),
            "emoji_search": true,
            "emoji_filter_ready": state.emoji_picker.popover.child().is_some(),
            "emoji_popup_visible": state.emoji_picker.popover.is_visible(),
            "emoji_query": state.emoji_picker.search.text().to_string(),
            "emoji_choice_count": state.emoji_picker.source_choice_count(),
            "emoji_visible_choice_count": state.emoji_picker.visible_choice_count(),
            "emoji_first_visible_name": state.emoji_picker.first_visible_name(),
            "emoji_contains_late_custom": state.emoji_picker.contains("late_status_parrot"),
            "emoji_selected_name": state.emoji_picker.selected_name(),
            "emoji_selected_visible_name": state.emoji_picker.selected_visible_name(),
            "expiration_choice_count": state.expiration_choice_count,
            "save_enabled": state.dialog.is_response_enabled("save"),
            "clear_available": state.dialog.has_response("clear"),
            "status_has_value": !state.status_entry.text().trim().is_empty()
                || !state.emoji_picker.selected_name().is_empty(),
            "header_title": imp.workspace_title_label.title().to_string(),
            "header_subtitle": imp.workspace_title_label.subtitle().to_string(),
            "window_width": window.width(),
        })
        .to_string(),
    );
}

enum TestComposerCompletion<'a> {
    Emoji(&'a str),
    Mention {
        user_id: &'a str,
        serialized: &'a str,
    },
}

fn record_test_composer_completion_ready(target: ComposerTarget, completion: &ComposerCompletion) {
    let Some(path) = std::env::var_os("CONDUIT_TEST_COMPOSER_COMPLETION_FILE") else {
        return;
    };
    if !completion.popover.is_visible() {
        return;
    }
    let target = match target {
        ComposerTarget::Message => "message",
        ComposerTarget::Thread => "thread",
    };
    let selected_index = completion
        .list
        .selected_row()
        .map_or(0, |row| row.index().max(0) as usize);
    let (kind, query, selected) = match (
        completion.token.as_ref(),
        completion.entries.get(selected_index),
    ) {
        (
            Some(ComposerCompletionToken::Emoji(token)),
            Some(ComposerCompletionEntry::Emoji(entry)),
        ) => ("emoji", token.query.as_str(), entry.name.as_str()),
        (
            Some(ComposerCompletionToken::Mention(token)),
            Some(ComposerCompletionEntry::Mention(candidate)),
        ) => ("mention", token.query.as_str(), candidate.user_id.as_str()),
        _ => return,
    };
    let _ = std::fs::write(
        path,
        serde_json::json!({
            "ready": kind,
            "query": query,
            "selected": selected,
            "count": completion.entries.len(),
            "target": target,
        })
        .to_string(),
    );
}

fn record_test_composer_completion(
    window: &imp::ConduitWindow,
    target: ComposerTarget,
    completion: TestComposerCompletion<'_>,
) {
    let Some(path) = std::env::var_os("CONDUIT_TEST_COMPOSER_COMPLETION_FILE") else {
        return;
    };
    let settings = window
        .message_view
        .borrow()
        .as_ref()
        .and_then(webkit6::prelude::WebViewExt::settings);
    let Some(settings) = settings else {
        return;
    };
    let target = match target {
        ComposerTarget::Message => "message",
        ComposerTarget::Thread => "thread",
    };
    let mut state = serde_json::json!({
        "target": target,
        "webkit": {
            "allow_file_access": settings.allows_file_access_from_file_urls(),
            "allow_universal_access": settings.allows_universal_access_from_file_urls(),
            "html5_database": settings.enables_html5_database(),
            "html5_local_storage": settings.enables_html5_local_storage(),
            "javascript": settings.enables_javascript(),
            "media": settings.enables_media(),
            "webaudio": settings.enables_webaudio(),
            "webgl": settings.enables_webgl(),
            "zoom_text_only": settings.is_zoom_text_only(),
        },
    });
    match completion {
        TestComposerCompletion::Emoji(name) => state["emoji"] = name.into(),
        TestComposerCompletion::Mention {
            user_id,
            serialized,
        } => {
            state["mention"] = user_id.into();
            state["serialized"] = serialized.into();
        }
    }
    let _ = std::fs::write(path, state.to_string());
}

fn huddle_device_row(label: &str, dropdown: &gtk::DropDown) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    row.append(dropdown);
    row
}

fn update_huddle_device_picker(
    picker: &HuddleDevicePicker,
    kind: HuddleDeviceKind,
    devices: &[HuddleDevice],
    selected_id: Option<&str>,
) {
    let matching = devices
        .iter()
        .filter(|device| device.kind == kind)
        .collect::<Vec<_>>();
    let labels = if matching.is_empty() {
        vec![gettext("System default")]
    } else {
        matching.iter().map(|device| device.label.clone()).collect()
    };
    let ids = matching
        .iter()
        .map(|device| device.id.clone())
        .collect::<Vec<_>>();
    let selected = matching
        .iter()
        .position(|device| selected_id == Some(device.id.as_str()))
        .or_else(|| matching.iter().position(|device| device.is_default))
        .unwrap_or_default() as u32;
    let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
    let model = gtk::StringList::new(&label_refs);

    picker.updating.set(true);
    *picker.ids.borrow_mut() = ids;
    picker.dropdown.set_model(Some(&model));
    picker.dropdown.set_selected(selected);
    picker.updating.set(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_user_directory_is_requested_only_by_consumers() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let authenticated = production
            .split_once("            RuntimeEventKind::Authenticated(auth) => {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::WorkspacePatch(patch) => {")
            .unwrap()
            .0;
        let switcher = production
            .split_once("    fn show_conversation_switcher(&self) {")
            .unwrap()
            .1
            .split_once("    fn request_user_directory(&self) {")
            .unwrap()
            .0;
        let directory_request = production
            .split_once("    fn request_user_directory(&self) {")
            .unwrap()
            .1
            .split_once("    fn show_change_status_dialog(&self) {")
            .unwrap()
            .0;
        let people_picker = production
            .split_once("    fn show_people_picker<F>(")
            .unwrap()
            .1
            .split_once("    fn show_new_channel_dialog(&self) {")
            .unwrap()
            .0;
        let composer_completion = production
            .split_once("    fn refresh_composer_completion(&self, target: ComposerTarget) {")
            .unwrap()
            .1
            .split_once("    fn dismiss_composer_completion(&self, target: ComposerTarget) {")
            .unwrap()
            .0;

        assert!(!authenticated.contains("DiscoverConversations"));
        assert!(!authenticated.contains("LoadUserDirectory"));
        assert!(switcher.contains("self.request_user_directory();"));
        assert!(directory_request.contains("RuntimeCommand::LoadUserDirectory"));
        assert!(people_picker.contains("request_user_directory"));
        assert!(composer_completion.contains("request_user_directory"));
    }

    #[test]
    fn catalog_sync_never_fans_out_per_user_requests() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let catalog_sync = production
            .split_once("    fn sync_conversations_from_catalog(&self) {")
            .unwrap()
            .1
            .split_once("    fn populate_user_names(&self")
            .unwrap()
            .0;

        assert!(!catalog_sync.contains("request_user_names"));
        assert!(!catalog_sync.contains("request_user_ids"));
        assert!(!catalog_sync.contains("RuntimeCommand::LoadUser"));
        assert!(!production.contains("fn request_conversation_user_names"));
    }

    #[test]
    fn implicit_user_lookups_wait_for_the_authoritative_directory() {
        let known_user_ids = HashSet::from(["U_KNOWN".to_string()]);
        let directory_user_ids = HashSet::from(["U_NAMELESS".to_string()]);
        let pending_user_ids = HashSet::new();
        let requested = vec![
            "U_KNOWN".to_string(),
            "U_NAMELESS".to_string(),
            "U_MISSING".to_string(),
            "U_MISSING".to_string(),
        ];

        let initial = plan_user_id_lookups(
            requested.clone(),
            &known_user_ids,
            &directory_user_ids,
            &pending_user_ids,
            false,
            false,
            false,
        );
        assert_eq!(
            initial,
            UserLookupPlan {
                deferred_ids: vec!["U_MISSING".to_string(), "U_NAMELESS".to_string()],
                individual_ids: Vec::new(),
                request_directory: true,
            }
        );

        let while_loading = plan_user_id_lookups(
            requested.clone(),
            &known_user_ids,
            &directory_user_ids,
            &pending_user_ids,
            false,
            false,
            true,
        );
        assert!(!while_loading.request_directory);
        assert!(while_loading.individual_ids.is_empty());

        let after_directory = plan_user_id_lookups(
            requested,
            &known_user_ids,
            &directory_user_ids,
            &pending_user_ids,
            true,
            false,
            false,
        );
        assert_eq!(after_directory.deferred_ids, Vec::<String>::new());
        assert_eq!(after_directory.individual_ids, vec!["U_MISSING"]);
        assert!(!after_directory.request_directory);

        let while_refreshing = plan_user_id_lookups(
            vec!["U_NEW".to_string()],
            &known_user_ids,
            &directory_user_ids,
            &pending_user_ids,
            true,
            false,
            true,
        );
        assert_eq!(while_refreshing.deferred_ids, vec!["U_NEW"]);
        assert!(while_refreshing.individual_ids.is_empty());

        let after_failed_refresh = plan_user_id_lookups(
            vec!["U_NEW".to_string()],
            &known_user_ids,
            &directory_user_ids,
            &pending_user_ids,
            true,
            true,
            false,
        );
        assert_eq!(after_failed_refresh.deferred_ids, vec!["U_NEW"]);
        assert!(after_failed_refresh.individual_ids.is_empty());
        assert!(!after_failed_refresh.request_directory);
    }

    #[test]
    fn bulk_directory_members_never_trigger_users_info_without_a_display_name() {
        let users = vec![SlackUser {
            id: Some("U_NAMELESS".to_string()),
            ..Default::default()
        }];
        let directory_user_ids = discovered_user_ids(&users);
        let plan = plan_user_id_lookups(
            vec!["U_NAMELESS".to_string()],
            &HashSet::new(),
            &directory_user_ids,
            &HashSet::new(),
            true,
            false,
            false,
        );

        assert!(plan.individual_ids.is_empty());
    }

    #[test]
    fn authoritative_directory_identity_maps_remove_users_and_cleared_values() {
        let users = vec![
            SlackUser {
                id: Some("U_KEEP".to_string()),
                name: Some("keep".to_string()),
                profile: Some(SlackUserProfile {
                    display_name: Some("Kept Name".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_CLEARED".to_string()),
                ..Default::default()
            },
        ];
        let replacement = user_directory_identity_maps(&users);

        assert_eq!(
            replacement.names,
            HashMap::from([("U_KEEP".to_string(), "Kept Name".to_string())])
        );
        assert!(replacement.full_names.is_empty());
        assert!(replacement.avatar_urls.is_empty());

        let current_names = HashMap::from([
            ("U_KEEP".to_string(), "Old Name".to_string()),
            ("U_CLEARED".to_string(), "Stale Name".to_string()),
            ("U_REMOVED".to_string(), "Removed Name".to_string()),
        ]);
        assert_eq!(
            changed_map_keys(&current_names, &replacement.names),
            vec![
                "U_CLEARED".to_string(),
                "U_KEEP".to_string(),
                "U_REMOVED".to_string(),
            ]
        );
        assert_eq!(
            changed_map_keys(
                &HashMap::from([("U_CLEARED".to_string(), "stale-avatar".to_string())]),
                &replacement.avatar_urls,
            ),
            vec!["U_CLEARED".to_string()]
        );
    }

    #[test]
    fn bulk_directory_event_replaces_identity_maps_while_incremental_events_upsert() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let directory_result = production
            .split_once("            RuntimeEventKind::ConversationPeopleDiscovered(users) => {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::ConversationOpened")
            .unwrap()
            .0;
        let user_loaded = production
            .split_once("            RuntimeEventKind::UserLoaded {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::UserProfileLoaded")
            .unwrap()
            .0;

        assert!(directory_result.contains("replace_user_directory_identity_maps(&users)"));
        assert!(!directory_result.contains("populate_user_names"));
        assert!(!directory_result.contains("populate_user_full_names"));
        assert!(!directory_result.contains("populate_user_avatar_urls"));
        assert!(user_loaded.contains("populate_user_names"));
        assert!(user_loaded.contains("populate_user_full_names"));
        assert!(user_loaded.contains("populate_user_avatar_urls"));
    }

    #[test]
    fn implicit_lookup_deferral_is_completed_and_reset_on_every_terminal_path() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let directory_result = production
            .split_once("            RuntimeEventKind::ConversationPeopleDiscovered(users) => {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::ConversationOpened")
            .unwrap()
            .0;
        let error_handler = production
            .split_once("            RuntimeEventKind::Error(error) => {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::RuntimeStartFailed")
            .unwrap()
            .0;
        let reset = production
            .split_once("    fn reset_workspace_state(&self) {")
            .unwrap()
            .1
            .split_once("    fn show_workspace(&self, auth: AuthInfo) {")
            .unwrap()
            .0;
        let profile = production
            .split_once("    fn show_user_profile(&self, user_id: &str) {")
            .unwrap()
            .1
            .split_once("    fn ")
            .unwrap()
            .0;

        assert!(directory_result.contains("resolve_deferred_user_ids"));
        assert!(!error_handler.contains("resolve_deferred_user_ids"));
        assert!(error_handler.contains("user_directory_request_in_flight.set(false)"));
        assert!(reset.contains("deferred_user_ids.borrow_mut().clear()"));
        assert!(reset.contains("user_directory_request_in_flight.set(false)"));
        assert!(profile.contains("RuntimeCommand::LoadUserProfile"));
    }

    #[test]
    fn people_picker_candidates_filter_and_sort_directory_users() {
        let users = vec![
            SlackUser {
                id: Some("U_CURRENT".to_string()),
                name: Some("current".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_GRACE".to_string()),
                real_name: Some("Grace Hopper".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_ADA".to_string()),
                real_name: Some("Ada Lovelace".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_EXCLUDED".to_string()),
                name: Some("excluded".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_DELETED".to_string()),
                name: Some("deleted".to_string()),
                deleted: Some(true),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_BOT".to_string()),
                name: Some("bot".to_string()),
                is_bot: Some(true),
                ..Default::default()
            },
        ];
        let excluded = HashSet::from(["U_EXCLUDED".to_string()]);

        assert_eq!(
            people_picker_candidates(&users, Some("U_CURRENT"), &excluded),
            vec![
                PeoplePickerCandidate {
                    user_id: "U_ADA".to_string(),
                    name: "Ada Lovelace".to_string(),
                },
                PeoplePickerCandidate {
                    user_id: "U_GRACE".to_string(),
                    name: "Grace Hopper".to_string(),
                },
            ]
        );
    }

    #[test]
    fn realtime_user_updates_merge_into_the_discovered_directory() {
        let mut users = vec![SlackUser {
            id: Some("U_ADA".to_string()),
            name: Some("ada".to_string()),
            real_name: Some("Ada Lovelace".to_string()),
            profile: Some(SlackUserProfile {
                display_name: Some("Ada".to_string()),
                email: Some("ada@example.test".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }];

        assert!(merge_discovered_user_update(
            &mut users,
            SlackUser {
                id: Some("U_ADA".to_string()),
                profile: Some(SlackUserProfile {
                    display_name: Some("Ada Byron".to_string()),
                    status_text: Some("Focusing".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ));

        assert_eq!(users.len(), 1);
        let user = &users[0];
        assert_eq!(user.name.as_deref(), Some("ada"));
        assert_eq!(user.real_name.as_deref(), Some("Ada Lovelace"));
        let profile = user.profile.as_ref().unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("Ada Byron"));
        assert_eq!(profile.email.as_deref(), Some("ada@example.test"));
        assert_eq!(profile.status_text.as_deref(), Some("Focusing"));

        assert!(merge_discovered_user_update(
            &mut users,
            SlackUser {
                id: Some("U_GRACE".to_string()),
                real_name: Some("Grace Hopper".to_string()),
                ..Default::default()
            },
        ));
        assert_eq!(users.len(), 2);
        assert!(!merge_discovered_user_update(
            &mut users,
            SlackUser {
                id: Some("U_GRACE".to_string()),
                real_name: Some("Grace Hopper".to_string()),
                ..Default::default()
            },
        ));
    }

    #[test]
    fn composer_directory_check_is_gated_per_active_mention_token() {
        assert_eq!(
            composer_directory_check_transition(Some(4), None),
            (Some(4), true)
        );
        assert_eq!(
            composer_directory_check_transition(Some(4), Some(4)),
            (Some(4), false),
            "typing more characters in the same mention must not enqueue another check"
        );
        assert_eq!(
            composer_directory_check_transition(Some(18), Some(4)),
            (Some(18), true),
            "moving to a different mention token starts one freshness check"
        );
        assert_eq!(
            composer_directory_check_transition(None, Some(18)),
            (None, false),
            "leaving mention completion resets the per-token gate"
        );
        assert_eq!(
            composer_directory_check_transition(Some(18), None),
            (Some(18), true),
            "a later mention session checks freshness again"
        );
    }

    #[test]
    fn only_failed_empty_workspace_directory_requests_enter_failure_state() {
        let workspace_user =
            OperationContext::new(RuntimeOperation::User, RuntimeTarget::Workspace);
        let profile_user = OperationContext::new(
            RuntimeOperation::User,
            RuntimeTarget::User("U1".to_string()),
        );
        let workspace_history =
            OperationContext::new(RuntimeOperation::History, RuntimeTarget::Workspace);

        assert!(failed_user_directory_request(&workspace_user, true));
        assert!(!failed_user_directory_request(&workspace_user, false));
        assert!(!failed_user_directory_request(&profile_user, true));
        assert!(!failed_user_directory_request(&workspace_history, true));
    }

    #[test]
    fn empty_people_picker_distinguishes_loading_loaded_and_failed_states() {
        let loading = people_picker_directory_state(false, false);
        let loaded = people_picker_directory_state(true, false);
        let failed = people_picker_directory_state(false, true);

        assert_eq!(loading, PeoplePickerDirectoryState::Loading);
        assert_eq!(people_picker_empty_message(loading), "Loading people...");
        assert_eq!(loaded, PeoplePickerDirectoryState::LoadedEmpty);
        assert_eq!(people_picker_empty_message(loaded), "No people available");
        assert_eq!(failed, PeoplePickerDirectoryState::Failed);
        assert_eq!(people_picker_empty_message(failed), "Could not load people");
        assert_eq!(
            people_picker_directory_state(true, true),
            PeoplePickerDirectoryState::Failed,
            "a failed refresh must not be hidden by an earlier empty result"
        );
    }

    #[test]
    fn failed_people_picker_load_exposes_retry_and_restores_loading_state() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let error_handler = production
            .split_once("            RuntimeEventKind::Error(error) => {")
            .unwrap()
            .1
            .split_once("            RuntimeEventKind::RuntimeStartFailed")
            .unwrap()
            .0;
        let directory_request = production
            .split_once("    fn request_user_directory(&self) {")
            .unwrap()
            .1
            .split_once("    fn show_change_status_dialog(&self) {")
            .unwrap()
            .0;
        let failure_view = production
            .split_once("    fn append_people_picker_load_failure(&self")
            .unwrap()
            .1
            .split_once("    fn show_new_channel_dialog(&self)")
            .unwrap()
            .0;

        assert!(error_handler.contains("user_directory_failed.set(true)"));
        assert!(error_handler.contains("refresh_open_people_picker"));
        assert!(directory_request.contains("user_directory_failed.set(false)"));
        assert!(directory_request.contains("refresh_open_people_picker"));
        assert!(failure_view.contains("Button::with_label(&gettext(\"Retry\"))"));
        assert!(failure_view.contains("request_user_directory"));
    }

    #[test]
    fn message_web_view_context_menu_classifies_unsupported_navigation_actions() {
        for action in [
            webkit6::ContextMenuAction::GoBack,
            webkit6::ContextMenuAction::GoForward,
            webkit6::ContextMenuAction::Stop,
            webkit6::ContextMenuAction::Reload,
        ] {
            assert!(message_web_view_context_menu_action_is_unsupported(action));
        }

        for action in [
            webkit6::ContextMenuAction::Copy,
            webkit6::ContextMenuAction::SelectAll,
            webkit6::ContextMenuAction::OpenLink,
            webkit6::ContextMenuAction::CopyLinkToClipboard,
        ] {
            assert!(!message_web_view_context_menu_action_is_unsupported(action));
        }
    }

    #[test]
    fn message_web_view_factory_filters_native_navigation_actions() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let factory = production
            .split_once("    fn create_message_web_view(")
            .unwrap()
            .1
            .split_once("    fn reaction_emoji_picker_model(")
            .unwrap()
            .0;

        assert!(factory.contains("connect_context_menu"));
        assert!(factory.contains("prune_message_web_view_context_menu(context_menu)"));
    }

    #[test]
    fn gtk_catalogs_accept_mutations_only_through_workspace_patches() {
        let production = include_str!("window.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let compact = production.split_whitespace().collect::<String>();
        for legacy_path in [
            "legacy_conversation_catalog_mutation_allowed",
            "populate_conversations(",
            "apply_conversation_left(",
            "apply_conversation_unread_state(",
            "apply_attention_observations(",
            "local_read_ts_by_channel",
            "pending_opened_conversation_ids",
            "workspace.conversations.borrow",
            "workspace.threads.borrow",
        ] {
            assert!(
                !compact.contains(legacy_path),
                "legacy GTK catalog mutation path remains: {legacy_path}"
            );
        }
    }

    #[test]
    fn composer_completion_description_announces_person_and_position() {
        let entry = ComposerCompletionEntry::Mention(MentionCandidate {
            user_id: "U1".to_string(),
            display_name: "Ada".to_string(),
            full_name: Some("Ada Lovelace".to_string()),
            username: Some("ada.dev".to_string()),
            search_aliases: Vec::new(),
        });

        assert_eq!(
            composer_completion_description(&entry, 1, 4),
            "Person suggestion 2 of 4: Ada, Ada Lovelace, @ada.dev"
        );
    }

    #[test]
    fn navigation_history_keeps_distinct_bounded_visits() {
        let mut history = Vec::new();
        let unreads = MainNavigationTarget::Unreads;
        remember_navigation(
            &mut history,
            MainNavigationTarget::Conversation("C1".into()),
            &unreads,
        );
        remember_navigation(
            &mut history,
            MainNavigationTarget::Conversation("C1".into()),
            &unreads,
        );
        assert_eq!(
            history,
            vec![MainNavigationTarget::Conversation("C1".into())]
        );

        for index in 0..=MAX_NAVIGATION_HISTORY {
            remember_navigation(
                &mut history,
                MainNavigationTarget::Conversation(format!("C{index}")),
                &unreads,
            );
        }
        assert_eq!(history.len(), MAX_NAVIGATION_HISTORY);
        assert_eq!(
            history.last(),
            Some(&MainNavigationTarget::Conversation("C100".into()))
        );
    }

    #[test]
    fn channel_names_follow_slack_creation_rules() {
        assert!(valid_channel_name("project_alpha-2"));
        assert!(valid_channel_name("  general  "));
        assert!(!valid_channel_name(""));
        assert!(!valid_channel_name("Project Alpha"));
        assert!(!valid_channel_name("127.0.0.1"));
        assert!(!valid_channel_name(&"a".repeat(81)));
    }

    #[test]
    fn reaction_picker_escape_fallback_uses_the_shared_cancel_event() {
        assert!(CANCEL_REACTION_PICKER_SCRIPT.contains("picker.open"));
        assert!(CANCEL_REACTION_PICKER_SCRIPT
            .contains("picker.dispatchEvent(new Event(\"cancel\", { cancelable: true }))"));
    }

    #[test]
    fn emoji_picker_bridge_accepts_only_the_typed_query_shape() {
        let query = emoji_picker_query_from_json(
            r#"{"version":1,"generation":42,"query":"party parr","category":null,"offset":64}"#,
        )
        .unwrap();

        assert_eq!(query.version, 1);
        assert_eq!(query.generation, 42);
        assert_eq!(query.query, "party parr");
        assert_eq!(query.category, None);
        assert_eq!(query.offset, 64);
        assert!(emoji_picker_query_from_json(
            r#"{"version":1,"generation":42,"query":"","category":null,"offset":0,"extra":true}"#
        )
        .is_none());
        assert!(emoji_picker_query_from_json("not-json").is_none());
    }

    #[test]
    fn emoji_picker_bridge_passes_serialized_data_as_a_function_argument() {
        assert_eq!(
            APPLY_EMOJI_PICKER_RESULT_SCRIPT,
            "window.conduitReceiveEmojiPickerResult(JSON.parse(payload));"
        );
        assert!(!APPLY_EMOJI_PICKER_RESULT_SCRIPT.contains("${"));
        assert!(!APPLY_EMOJI_PICKER_RESULT_SCRIPT.contains("entries"));
    }

    #[test]
    fn lifecycle_presentation_owns_connection_surface_and_status() {
        let cases = [
            (
                WorkspaceLifecycle::Disconnected,
                WorkspaceLifecycleSurface::Connect,
                "Choose a workspace to continue",
                false,
                false,
            ),
            (
                WorkspaceLifecycle::Connecting,
                WorkspaceLifecycleSurface::Loading,
                "Connecting to Slack…",
                false,
                false,
            ),
            (
                WorkspaceLifecycle::Syncing,
                WorkspaceLifecycleSurface::Loading,
                "Syncing workspace…",
                false,
                false,
            ),
            (
                WorkspaceLifecycle::Ready,
                WorkspaceLifecycleSurface::Workspace,
                "",
                false,
                true,
            ),
            (
                WorkspaceLifecycle::Degraded,
                WorkspaceLifecycleSurface::Workspace,
                "Connection interrupted. Retrying…",
                true,
                true,
            ),
            (
                WorkspaceLifecycle::AuthenticationRequired,
                WorkspaceLifecycleSurface::Connect,
                "Slack authentication failed. Sign in again.",
                false,
                false,
            ),
            (
                WorkspaceLifecycle::StartupFailed,
                WorkspaceLifecycleSurface::Connect,
                "Conduit could not start.",
                false,
                false,
            ),
        ];

        for (lifecycle, surface, status, initial_sync_complete, workspace_interactive) in cases {
            assert_eq!(
                workspace_lifecycle_presentation(lifecycle, true, initial_sync_complete),
                WorkspaceLifecyclePresentation {
                    surface,
                    status,
                    workspace_interactive,
                }
            );
        }

        assert_eq!(
            workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, false, false).surface,
            WorkspaceLifecycleSurface::Connect
        );
    }

    #[test]
    fn initial_sync_uses_movable_loading_surface_but_recovery_keeps_workspace() {
        let initial_sync =
            workspace_lifecycle_presentation(WorkspaceLifecycle::Syncing, true, false);
        assert_eq!(initial_sync.surface, WorkspaceLifecycleSurface::Loading);
        assert!(!initial_sync.workspace_interactive);

        let ready = workspace_lifecycle_presentation(WorkspaceLifecycle::Ready, true, false);
        assert!(ready.workspace_interactive);

        let recovery = workspace_lifecycle_presentation(WorkspaceLifecycle::Syncing, true, true);
        assert!(recovery.workspace_interactive);
        let degraded = workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, true, true);
        assert!(degraded.workspace_interactive);

        let initial_failure =
            workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, true, false);
        assert_eq!(initial_failure.surface, WorkspaceLifecycleSurface::Loading);
        assert!(!initial_failure.workspace_interactive);
    }

    #[test]
    fn initial_sync_completion_is_latched_only_for_the_active_session() {
        assert!(!initial_sync_completion(false, WorkspaceLifecycle::Syncing));
        assert!(initial_sync_completion(false, WorkspaceLifecycle::Ready));
        assert!(initial_sync_completion(true, WorkspaceLifecycle::Syncing));
        assert!(initial_sync_completion(true, WorkspaceLifecycle::Degraded));
        assert!(!initial_sync_completion(
            true,
            WorkspaceLifecycle::AuthenticationRequired
        ));
        assert!(!initial_sync_completion(
            true,
            WorkspaceLifecycle::Disconnected
        ));
    }

    #[test]
    fn membership_reactivation_ignores_first_activation_and_repeated_notifications() {
        let mut state = MembershipReactivationState::default();

        assert!(!state.observe(false));
        assert!(!state.observe(true));
        assert!(!state.observe(true));
        assert!(!state.observe(false));
        assert!(state.observe(true));
        assert!(!state.observe(true));
    }

    #[test]
    fn membership_reactivation_requires_a_workspace_and_absent_realtime_transport() {
        assert!(!membership_reactivation_should_refresh(
            true,
            false,
            RealtimeStatus::default(),
        ));
        assert!(membership_reactivation_should_refresh(
            true,
            true,
            RealtimeStatus::default(),
        ));
        assert!(membership_reactivation_should_refresh(
            true,
            true,
            RealtimeStatus::configuration_error(),
        ));

        for status in [
            RealtimeStatus::connecting(crate::realtime::RealtimeTransport::SocketMode),
            RealtimeStatus::online(crate::realtime::RealtimeTransport::SocketMode),
            RealtimeStatus::reconnecting(crate::realtime::RealtimeTransport::SocketMode),
        ] {
            assert!(!membership_reactivation_should_refresh(true, true, status));
        }
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

    #[test]
    fn timeline_lifecycle_actions_require_a_valid_generation() {
        assert_eq!(
            timeline_lifecycle_action(
                &url::Url::parse("conduit://timeline-positioned?generation=42").unwrap(),
            ),
            Some(TimelineLifecycleAction::Positioned(42))
        );
        assert_eq!(
            timeline_lifecycle_action(
                &url::Url::parse("conduit://timeline-interacted?generation=43").unwrap(),
            ),
            Some(TimelineLifecycleAction::Interacted(43))
        );
        for uri in [
            "conduit://timeline-positioned",
            "conduit://timeline-positioned?generation=0",
            "conduit://timeline-positioned?generation=oops",
            "conduit://other?generation=42",
        ] {
            assert_eq!(
                timeline_lifecycle_action(&url::Url::parse(uri).unwrap()),
                None,
                "{uri}"
            );
        }
    }

    fn sidebar_row(id: &str, title: &str) -> SidebarRowModel {
        SidebarRowModel {
            id: id.to_string(),
            title: title.to_string(),
            kind: ConversationKind::DirectMessage,
            unread: false,
            unread_count: 0,
            selected: false,
            starred: false,
            private: true,
            muted: false,
            external: false,
            huddle_active: false,
            search_aliases: Vec::new(),
            status: None,
        }
    }

    fn picker_item(id: &str, title: &str) -> ConversationPickerItem {
        ConversationPickerItem {
            row: sidebar_row(id, title),
            action: ConversationPickerAction::OpenConversation,
        }
    }

    #[test]
    fn picker_population_flattens_sections_and_preserves_bounded_order() {
        let sections = ConversationPickerSections {
            conversations: (0..30)
                .map(|index| picker_item(&format!("C{index}"), &format!("Channel {index}")))
                .collect(),
            channels: vec![picker_item("C_JOIN", "Join me")],
            people: vec![picker_item("U1", "Ada")],
            search_results: None,
        };
        let entries = conversation_picker_population_entries(&sections);
        assert!(matches!(
            entries.front(),
            Some(ConversationPickerListEntry::Header(title)) if title == "Conversations"
        ));
        assert!(entries.iter().any(
            |entry| matches!(entry, ConversationPickerListEntry::Header(title) if title == "Channels you can join")
        ));
        assert!(entries.iter().any(
            |entry| matches!(entry, ConversationPickerListEntry::Header(title) if title == "People")
        ));

        let expected = entries.iter().cloned().collect::<Vec<_>>();
        let mut population = ConversationPickerPopulation::new(7, entries);
        let mut actual = Vec::new();
        while let Some(batch) = population.next_batch(7) {
            assert!(batch.len() <= PICKER_POPULATION_BATCH_SIZE);
            actual.extend(batch);
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn picker_population_rejects_stale_generation_without_appending() {
        let sections = ConversationPickerSections {
            search_results: Some(vec![picker_item("C1", "General")]),
            ..Default::default()
        };
        let entries = conversation_picker_population_entries(&sections);
        assert!(entries
            .iter()
            .all(|entry| matches!(entry, ConversationPickerListEntry::Item(_))));

        let mut population = ConversationPickerPopulation::new(4, entries);
        assert_eq!(population.next_batch(5), None);
        assert!(population.is_empty());
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
    fn request_coordinator_accepts_a_session_scoped_recovered_workspace_patch() {
        let mut coordinator = RequestCoordinator::default();
        let first = coordinator.begin_session(&RuntimeCommand::RefreshConversations);
        let second = coordinator.issue(&RuntimeCommand::RefreshConversations);
        let patch = crate::workspace_pipeline::WorkspacePatch::new(
            crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
            vec![
                crate::workspace_pipeline::WorkspaceChange::ConversationUpsert(SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }),
            ],
        )
        .unwrap();
        let recovered = RuntimeEvent {
            meta: RuntimeEventMeta {
                session: first.session,
                request: None,
                context: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
            },
            kind: RuntimeEventKind::WorkspacePatch(patch),
        };

        assert!(!coordinator.accepts(&RuntimeEventMeta::new(
            first,
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
        )));
        assert!(coordinator.accepts(&RuntimeEventMeta::new(
            second,
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
        )));
        assert!(coordinator.accepts(&recovered.meta));
        let state = WorkspaceSessionState::default();
        let RuntimeEventKind::WorkspacePatch(patch) = recovered.kind else {
            unreachable!();
        };
        state.apply_workspace_patch(&patch).unwrap();
        assert!(state.conversations().get("C1").is_some());
    }

    #[test]
    fn typed_workspace_patch_applies_on_its_owning_main_context() {
        let context = glib::MainContext::new();
        context.block_on(async {
            assert!(context.is_owner());
            let state = WorkspaceSessionState::default();
            let patch = crate::workspace_pipeline::WorkspacePatch::new(
                crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
                vec![
                    crate::workspace_pipeline::WorkspaceChange::ConversationUpsert(
                        SlackConversation {
                            id: "C1".to_string(),
                            name: Some("general".to_string()),
                            ..Default::default()
                        },
                    ),
                ],
            )
            .unwrap();
            let event = RuntimeEventKind::WorkspacePatch(patch);
            let RuntimeEventKind::WorkspacePatch(patch) = event else {
                unreachable!();
            };

            assert!(state
                .apply_workspace_patch(&patch)
                .unwrap()
                .conversation_changed());
            assert_eq!(
                state
                    .conversations()
                    .get("C1")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("general")
            );
        });
    }

    #[test]
    fn conversation_sync_completion_initializes_an_empty_workspace_once() {
        assert!(conversation_sync_completion_needs_catalog_sync(false));
        assert!(!conversation_sync_completion_needs_catalog_sync(true));
    }

    #[test]
    fn typed_private_conversation_removal_preserves_discovery_cleanup() {
        let state = WorkspaceSessionState::default();
        let private = SlackConversation {
            id: "C_PRIVATE".to_string(),
            is_channel: Some(true),
            is_private: Some(true),
            ..Default::default()
        };
        let public = SlackConversation {
            id: "C_PUBLIC".to_string(),
            is_channel: Some(true),
            ..Default::default()
        };
        let first_revision = crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor();
        state
            .apply_workspace_patch(
                &crate::workspace_pipeline::WorkspacePatch::new(
                    first_revision,
                    vec![
                        crate::workspace_pipeline::WorkspaceChange::ConversationsReset(vec![
                            private.clone(),
                            public.clone(),
                        ]),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        let removal = state
            .apply_workspace_patch(
                &crate::workspace_pipeline::WorkspacePatch::new(
                    first_revision.successor(),
                    vec![
                        crate::workspace_pipeline::WorkspaceChange::ConversationRemoved {
                            channel_id: private.id.clone(),
                        },
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        let mut discovered = vec![private, public];

        remove_patch_departures_from_discovery(&mut discovered, removal.removals());

        assert_eq!(
            discovered
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            vec!["C_PUBLIC"]
        );
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
                RuntimeOperation::Files,
                RuntimeTarget::File("F123".to_string()),
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
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel("C123".to_string()),
                RuntimeFailureRecovery::ConversationStar,
            ),
            (
                RuntimeOperation::UserStatus,
                RuntimeTarget::Workspace,
                RuntimeFailureRecovery::UserStatus,
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
    fn status_permission_failure_explains_profile_reauthorization() {
        let failure = RuntimeFailure {
            category: RuntimeFailureCategory::Validation,
            message: "Slack does not allow this action for this conversation.".to_string(),
        };

        let message = current_user_status_error_message(&failure);
        assert!(message.contains("change your status"));
        assert!(message.contains("profile access"));
        assert!(!message.contains("conversation"));
    }

    #[test]
    fn internal_status_failure_does_not_assume_reauthorization_is_needed() {
        let failure = RuntimeFailure {
            category: RuntimeFailureCategory::Internal,
            message: "unexpected response".to_string(),
        };

        let message = current_user_status_error_message(&failure);
        assert!(message.contains("Try again"));
        assert!(!message.contains("OAuth"));
        assert!(!message.contains("Reconnect"));
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
    fn sidebar_section_state_toggles_independently() {
        let mut collapsed = HashSet::from([SidebarSectionKind::Channels]);

        toggle_sidebar_section_state(&mut collapsed, SidebarSectionKind::DirectMessages);
        assert!(collapsed.contains(&SidebarSectionKind::Channels));
        assert!(collapsed.contains(&SidebarSectionKind::DirectMessages));

        toggle_sidebar_section_state(&mut collapsed, SidebarSectionKind::Channels);
        assert!(!collapsed.contains(&SidebarSectionKind::Channels));
        assert!(collapsed.contains(&SidebarSectionKind::DirectMessages));
    }

    #[test]
    fn sidebar_section_accessibility_describes_the_available_action() {
        assert_eq!(
            sidebar_section_accessible_label("Channels", false),
            "Collapse Channels"
        );
        assert_eq!(
            sidebar_section_accessible_label("Channels", true),
            "Expand Channels"
        );
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
    fn status_expiration_choices_resolve_to_absolute_slack_timestamps() {
        let now = 1_000;

        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Never, now, 2_000, 7_000),
            0
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Minutes30, now, 2_000, 7_000),
            2_800
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Hour1, now, 2_000, 7_000),
            4_600
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Hours4, now, 2_000, 7_000),
            15_400
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Today, now, 2_000, 7_000),
            2_000
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::ThisWeek, now, 2_000, 7_000),
            7_000
        );
        assert_eq!(
            status_expiration_for_choice(
                StatusExpirationChoice::Existing(3_500),
                now,
                2_000,
                7_000,
            ),
            3_500
        );
    }

    #[test]
    fn status_dialog_builds_text_only_and_emoji_only_statuses() {
        assert_eq!(
            status_from_dialog_input(
                " Focus time ",
                "",
                StatusExpirationChoice::Hour1,
                1_000,
                2_000,
                7_000,
            ),
            SlackUserStatus {
                text: "Focus time".to_string(),
                emoji: String::new(),
                expiration: 4_600,
            }
        );
        assert_eq!(
            status_from_dialog_input(
                "",
                ":headphones:",
                StatusExpirationChoice::Never,
                1_000,
                2_000,
                7_000,
            ),
            SlackUserStatus {
                text: String::new(),
                emoji: "headphones".to_string(),
                expiration: 0,
            }
        );
        assert_eq!(
            status_from_dialog_input(
                &"a".repeat(101),
                "",
                StatusExpirationChoice::Never,
                1_000,
                2_000,
                7_000,
            )
            .text
            .chars()
            .count(),
            100
        );
    }

    #[test]
    fn status_emoji_picker_searches_the_entire_compatible_source_without_truncation() {
        let custom = HashMap::from([(
            "party_parrot".to_string(),
            "https://emoji.example/party-parrot.gif".to_string(),
        )]);
        let model = StatusEmojiPickerModel::new(&custom, "");
        let all = model.search("");

        assert_eq!(all.len(), model.choice_count());
        assert_eq!(all.first().map(|choice| choice.name.as_str()), Some(""));
        assert!(all.iter().any(|choice| choice.name == "wales"));
        assert!(all.iter().any(|choice| choice.name == "party_parrot"));
        assert_eq!(
            model
                .search("PARTY parr")
                .first()
                .map(|choice| choice.name.as_str()),
            Some("party_parrot")
        );
    }

    #[test]
    fn status_emoji_picker_preserves_selection_and_prefers_workspace_collisions() {
        let selected = StatusEmojiPickerModel::new(&HashMap::new(), ":still_loading:");
        assert!(selected.contains("still_loading"));
        assert_eq!(
            selected.selected_label("still_loading"),
            ":still_loading: - still loading"
        );

        let custom = HashMap::from([(
            "rocket".to_string(),
            "https://emoji.example/custom-rocket.gif".to_string(),
        )]);
        let refreshed = StatusEmojiPickerModel::new(&custom, "still_loading");
        assert!(refreshed.contains("still_loading"));
        assert_eq!(
            refreshed
                .search("rocket")
                .first()
                .map(|choice| choice.label.as_str()),
            Some(":rocket: - rocket")
        );
    }

    #[test]
    fn status_dialog_keeps_clear_available_for_a_failed_clear_retry() {
        assert!(!status_dialog_clear_available(
            &SlackUserStatus::default(),
            100,
            false
        ));
        assert!(status_dialog_clear_available(
            &SlackUserStatus::default(),
            100,
            true
        ));
        assert!(status_dialog_clear_available(
            &SlackUserStatus {
                text: "Focus".to_string(),
                ..Default::default()
            },
            100,
            false
        ));
    }

    #[test]
    fn user_status_presentation_handles_text_unicode_custom_and_expiry() {
        let custom = HashMap::from([(
            "working_remotely".to_string(),
            "https://emoji.example/remote.png".to_string(),
        )]);

        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Focus time".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "Focus time".to_string(),
                accessible_text: "Focus time".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Focus time".to_string(),
                    emoji: ":headphones:".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "🎧 Focus time".to_string(),
                accessible_text: "Focus time".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Remote".to_string(),
                    emoji: ":working_remotely:".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "● Remote".to_string(),
                accessible_text: "Remote".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Expired".to_string(),
                    expiration: 100,
                    ..Default::default()
                },
                &custom,
                100,
            ),
            None
        );
    }

    #[test]
    fn sparse_profile_updates_preserve_status_while_explicit_blanks_clear_it() {
        let mut statuses = HashMap::from([(
            "U123".to_string(),
            SlackUserStatus {
                text: "Focus time".to_string(),
                emoji: ":headphones:".to_string(),
                expiration: 0,
            },
        )]);

        assert!(!apply_user_status_profile_update(
            &mut statuses,
            "U123",
            &SlackUserProfile {
                huddle_state_call_id: Some("R123".to_string()),
                ..Default::default()
            },
        ));
        assert_eq!(
            statuses.get("U123").map(|status| status.text.as_str()),
            Some("Focus time")
        );

        assert!(apply_user_status_profile_update(
            &mut statuses,
            "U123",
            &SlackUserProfile {
                status_text: Some(String::new()),
                status_emoji: Some(String::new()),
                status_expiration: Some(0),
                ..Default::default()
            },
        ));
        assert!(!statuses.contains_key("U123"));
    }

    #[test]
    fn status_snapshots_replace_stale_values_without_overwriting_newer_users() {
        let current_status = SlackUserStatus {
            text: "Current".to_string(),
            ..Default::default()
        };
        let stale_status = SlackUserStatus {
            text: "Stale".to_string(),
            ..Default::default()
        };
        let new_status = SlackUserStatus {
            text: "New".to_string(),
            ..Default::default()
        };
        let mut statuses = HashMap::from([
            ("U_CHANGED".to_string(), current_status.clone()),
            ("U_REMOVED".to_string(), stale_status.clone()),
        ]);

        let changed = apply_user_status_snapshot(
            &mut statuses,
            HashMap::from([
                ("U_CHANGED".to_string(), stale_status.clone()),
                ("U_NEW".to_string(), new_status.clone()),
            ]),
            true,
            &HashSet::from(["U_CHANGED".to_string()]),
        );

        assert_eq!(statuses.get("U_CHANGED"), Some(&current_status));
        assert_eq!(statuses.get("U_NEW"), Some(&new_status));
        assert!(!statuses.contains_key("U_REMOVED"));
        assert!(!changed.contains(&"U_CHANGED".to_string()));
        assert!(changed.contains(&"U_NEW".to_string()));
        assert!(changed.contains(&"U_REMOVED".to_string()));

        statuses.remove("U_CHANGED");
        apply_user_status_snapshot(
            &mut statuses,
            HashMap::from([
                ("U_CHANGED".to_string(), stale_status),
                ("U_NEW".to_string(), new_status.clone()),
            ]),
            true,
            &HashSet::from(["U_CHANGED".to_string()]),
        );
        assert!(!statuses.contains_key("U_CHANGED"));

        apply_user_status_snapshot(
            &mut statuses,
            HashMap::from([
                ("U_NEW".to_string(), current_status),
                ("U_CACHED".to_string(), new_status.clone()),
            ]),
            false,
            &HashSet::new(),
        );
        assert_eq!(statuses.get("U_NEW"), Some(&new_status));
        assert_eq!(statuses.get("U_CACHED"), Some(&new_status));
    }

    #[test]
    fn current_user_header_prefers_display_name_and_falls_back_to_workspace() {
        let names = HashMap::from([("U123".to_string(), "Vincent".to_string())]);

        assert_eq!(
            current_user_header_title(Some("U123"), &names, Some("Signicat")),
            "Vincent"
        );
        assert_eq!(
            current_user_header_title(Some("U999"), &names, Some("Signicat")),
            "Signicat"
        );
        assert_eq!(
            current_user_header_title(None, &names, None),
            gettext("Workspace")
        );
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
            "U123"
        ));
        assert!(sidebar_user_name_update_needs_render(
            &conversations,
            "U456"
        ));
        assert!(!sidebar_user_name_update_needs_render(
            &conversations,
            "U999"
        ));
        assert!(!sidebar_user_name_update_needs_render(&[], "U123"));
    }

    #[test]
    fn message_text_zoom_matches_the_gtk_theme_font_size() {
        let expected = 11.0 * 96.0 / 72.0 / 14.0;
        assert!((message_text_zoom(Some("Cantarell 11")) - expected).abs() < 1e-12);
        assert!((message_text_zoom(Some("Sans 10.5")) - 1.0).abs() < 1e-12);
        assert!((message_text_zoom(Some("Sans 14px")) - 1.0).abs() < 1e-12);
        assert_eq!(
            message_text_zoom(Some("Cantarell 11")),
            message_text_zoom(Some("Serif 11"))
        );
        assert_eq!(message_text_zoom(Some("Cantarell")), 1.0);
        assert_eq!(message_text_zoom(None), 1.0);
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
    fn attention_notification_last_mile_blocks_active_and_muted_conversations() {
        assert!(attention_notification_should_deliver(
            false,
            Some("C123"),
            "C123",
            false
        ));
        assert!(attention_notification_should_deliver(
            true,
            Some("C999"),
            "C123",
            false
        ));
        assert!(!attention_notification_should_deliver(
            true,
            Some("C123"),
            "C123",
            false
        ));
        assert!(!attention_notification_should_deliver(
            false, None, "C123", true
        ));
    }

    #[test]
    fn notification_conversation_waits_for_direct_message_names() {
        let direct_message = SlackConversation {
            id: "D123".into(),
            user: Some("U456".into()),
            is_im: Some(true),
            ..Default::default()
        };
        assert_eq!(
            message_notification_conversation(
                Some(&direct_message),
                &HashMap::new(),
                &HashMap::new(),
                Some("U123"),
            ),
            None
        );
        let resolved = message_notification_conversation(
            Some(&direct_message),
            &HashMap::from([("U456".into(), "Ada".into())]),
            &HashMap::new(),
            Some("U123"),
        )
        .unwrap();
        assert_eq!(resolved, ("Ada".into(), false));
        assert!(!resolved.0.contains("U456"));
    }

    #[test]
    fn notification_conversation_waits_for_every_group_dm_name() {
        let group_message: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "is_mpim": true,
            "users": ["U123", "U456", "U789"]
        }))
        .unwrap();
        assert!(message_notification_conversation(
            Some(&group_message),
            &HashMap::from([("U456".into(), "Ada".into())]),
            &HashMap::new(),
            Some("U123"),
        )
        .is_none());
        assert_eq!(
            message_notification_conversation(
                Some(&group_message),
                &HashMap::from([
                    ("U456".into(), "Ada".into()),
                    ("U789".into(), "Grace".into()),
                ]),
                &HashMap::new(),
                Some("U123"),
            ),
            Some(("Ada, Grace".into(), false))
        );
    }

    #[test]
    fn notification_body_uses_fallback_for_empty_message_text() {
        assert_eq!(
            message_notification_body(None, &HashMap::new()),
            Some("New message".into())
        );
        assert_eq!(
            message_notification_body(
                Some(&SlackMessage {
                    ts: "1710000100.000000".to_string(),
                    text: Some("   ".to_string()),
                    ..Default::default()
                }),
                &HashMap::new()
            ),
            Some("New message".into())
        );
        assert_eq!(
            message_notification_body(
                Some(&message("1710000200.000000", "Hello")),
                &HashMap::new()
            ),
            Some("Hello".into())
        );
    }

    #[test]
    fn notification_body_uses_attachment_text() {
        let message = SlackMessage {
            attachments: Some(vec![crate::models::SlackAttachment {
                text: Some("Review with <@U123>".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(
            message_notification_body(
                Some(&message),
                &HashMap::from([("U123".to_string(), "Ada".to_string())]),
            )
            .as_deref(),
            Some("Review with @Ada")
        );
    }

    #[test]
    fn channel_notification_content_waits_for_and_includes_sender_display_name() {
        let mut incoming = message("1710000200.000000", "Hello <@U789>");
        incoming.user = Some("U456".into());
        assert_eq!(
            message_notification_content("general", true, &incoming, &HashMap::new()),
            None
        );
        assert_eq!(
            message_notification_content(
                "general",
                true,
                &incoming,
                &HashMap::from([("U456".into(), "Ada".into())]),
            ),
            None
        );
        assert_eq!(
            message_notification_content(
                "general",
                true,
                &incoming,
                &HashMap::from([
                    ("U456".into(), "Ada".into()),
                    ("U789".into(), "Grace".into()),
                ]),
            ),
            Some(("general".into(), "Ada: Hello @Grace".into()))
        );
        assert_eq!(
            message_notification_content(
                "Ada",
                false,
                &incoming,
                &HashMap::from([("U789".into(), "Grace".into())]),
            ),
            Some(("Ada".into(), "Hello @Grace".into()))
        );
    }

    #[test]
    fn local_reaction_completion_keeps_the_incremental_update_payload() {
        assert_eq!(
            local_reaction_update("C123", "1710000200.000000", "eyes", true, Some("U123")),
            Some(ReactionUpdate {
                channel_id: "C123".into(),
                ts: "1710000200.000000".into(),
                name: "eyes".into(),
                user_id: "U123".into(),
                added: true,
            })
        );
        assert_eq!(
            local_reaction_update("C123", "1710000200.000000", "eyes", true, None),
            None
        );
    }

    #[test]
    fn notification_targets_wait_for_the_workspace_and_conversations() {
        let target = NotificationTarget {
            workspace_id: "T123".into(),
            channel_id: "C123".into(),
            thread_ts: Some("1710000000.000100".into()),
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
    fn conversation_targets_select_known_channels_and_open_prospective_dms() {
        let conversations = vec![SlackConversation {
            id: "D123".into(),
            user: Some("U123".into()),
            is_im: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            conversation_target_action("C123", &conversations),
            ConversationTargetAction::SelectConversation("C123".into())
        );
        assert_eq!(
            conversation_target_action("U123", &conversations),
            ConversationTargetAction::SelectConversation("D123".into())
        );
        assert_eq!(
            conversation_target_action("U456", &conversations),
            ConversationTargetAction::OpenDirectMessage("U456".into())
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
    fn pending_draft_deletion_forces_shutdown_persistence() {
        assert!(draft_persist_required(false, true));
        assert!(draft_persist_required(true, false));
        assert!(!draft_persist_required(false, false));
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
    fn sidebar_star_action_toggles_supported_conversations() {
        let public_channel = SlackConversation {
            is_channel: Some(true),
            ..Default::default()
        };
        let starred_direct_message = SlackConversation {
            is_im: Some(true),
            is_starred: Some(true),
            ..Default::default()
        };
        let unsupported = SlackConversation::default();

        let star = sidebar_conversation_star_action(&public_channel).unwrap();
        assert_eq!(star.label(), "Star");
        assert!(star.starred);

        let unstar = sidebar_conversation_star_action(&starred_direct_message).unwrap();
        assert_eq!(unstar.label(), "Unstar");
        assert!(!unstar.starred);

        assert_eq!(sidebar_conversation_star_action(&unsupported), None);
    }

    #[test]
    fn sidebar_profile_action_targets_only_one_to_one_dm_people() {
        let direct_message = SlackConversation {
            is_im: Some(true),
            user: Some("U123".into()),
            ..Default::default()
        };
        let group_direct_message = SlackConversation {
            is_mpim: Some(true),
            user: Some("U123".into()),
            ..Default::default()
        };
        let channel = SlackConversation {
            is_channel: Some(true),
            user: Some("U123".into()),
            ..Default::default()
        };
        let missing_user = SlackConversation {
            is_im: Some(true),
            ..Default::default()
        };
        let blank_user = SlackConversation {
            is_im: Some(true),
            user: Some("  ".into()),
            ..Default::default()
        };

        let action = sidebar_conversation_profile_action(&direct_message).unwrap();
        assert_eq!(action.label(), "Profile");
        assert_eq!(action.user_id, "U123");
        assert_eq!(
            sidebar_conversation_profile_action(&group_direct_message),
            None
        );
        assert_eq!(sidebar_conversation_profile_action(&channel), None);
        assert_eq!(sidebar_conversation_profile_action(&missing_user), None);
        assert_eq!(sidebar_conversation_profile_action(&blank_user), None);
    }

    #[test]
    fn sidebar_context_menu_opens_from_standard_keyboard_shortcuts() {
        assert!(sidebar_context_menu_key(
            gtk::gdk::Key::Menu,
            gtk::gdk::ModifierType::empty(),
        ));
        assert!(sidebar_context_menu_key(
            gtk::gdk::Key::F10,
            gtk::gdk::ModifierType::SHIFT_MASK,
        ));
        assert!(!sidebar_context_menu_key(
            gtk::gdk::Key::F10,
            gtk::gdk::ModifierType::empty(),
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
    fn window_template_preserves_adaptive_and_accessible_boundaries() {
        let template = include_str!("window.ui");

        for required in [
            "AdwNavigationSplitView\" id=\"workspace_split",
            "AdwOverlaySplitView\" id=\"thread_split",
            "AdwPasswordEntryRow\" id=\"xoxc_token_entry",
            "AdwPasswordEntryRow\" id=\"xoxd_token_entry",
            "GtkToggleButton\" id=\"sidebar_all_filter_button",
            "Show All Conversations",
            "GtkLabel\" id=\"message_status_label",
        ] {
            assert!(
                template.contains(required),
                "missing window contract {required}"
            );
        }

        let message_status = template
            .split_once("GtkLabel\" id=\"message_status_label\"")
            .and_then(|(_, rest)| rest.split_once("</object>"))
            .map(|(object, _)| object)
            .expect("message status label should be a complete template object");
        assert!(message_status.contains("<property name=\"accessible-role\">status</property>"));

        let status_action = template
            .find("<attribute name=\"action\">win.change-status</attribute>")
            .expect("workspace menu should expose the status dialog");
        let new_message_action = template
            .find("<attribute name=\"action\">win.new-message</attribute>")
            .expect("workspace menu should expose new message");
        assert!(status_action < new_message_action);
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
    fn local_arrival_repatches_a_socket_first_duplicate_on_the_visible_surface() {
        assert!(timeline_patch_needed(
            false,
            Some(TimelineMessageArrival::Sent),
            true
        ));
        assert!(!timeline_patch_needed(
            false,
            Some(TimelineMessageArrival::Sent),
            false
        ));
        assert!(!timeline_patch_needed(false, None, true));
        assert!(timeline_patch_needed(true, None, false));
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
    fn message_image_requests_include_each_cached_avatar_once() {
        let messages = [
            SlackMessage {
                user: Some("U123".to_string()),
                ..Default::default()
            },
            SlackMessage {
                user: Some("U123".to_string()),
                ..Default::default()
            },
        ];
        let avatar_url = "https://avatars.slack-edge.com/ada.png".to_string();
        let requests = message_image_asset_requests(
            &messages,
            &HashMap::from([("U123".to_string(), avatar_url.clone())]),
        );

        assert_eq!(requests, vec![(avatar_url.clone(), avatar_url)]);
    }

    #[test]
    fn message_image_requests_include_bot_and_attachment_images() {
        let bot_avatar = "https://avatars.slack-edge.com/bot.png".to_string();
        let attachment_image = "https://files.slack.com/request.png".to_string();
        let messages = [SlackMessage {
            bot_profile: Some(crate::models::SlackBotProfile {
                icons: Some(crate::models::SlackIcons {
                    image_72: Some(bot_avatar.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            attachments: Some(vec![crate::models::SlackAttachment {
                image_url: Some(attachment_image.clone()),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        assert_eq!(
            message_image_asset_requests(&messages, &HashMap::new()),
            vec![
                (bot_avatar.clone(), bot_avatar),
                (attachment_image.clone(), attachment_image),
            ]
        );
    }
}
