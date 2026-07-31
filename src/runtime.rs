use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

#[cfg(test)]
use crate::attention::AttentionReason;
use crate::attention::{AttentionDecision, AttentionPreferences, DeliveryState};
use crate::attention_metrics::{AttentionMetrics, AttentionPersistenceOutcome};
use crate::auth::{
    browser_session_token_from_env, browser_session_token_from_values, configured_app_token,
    OAuthConfig, SlackOAuthClient, TokenStore,
};
use crate::config;
use crate::huddles::coordinator::{CoordinatorInput, HuddleCoordinator, HuddleEffect};
use crate::huddles::model::{ActiveHuddle, HuddlePresence};
use crate::huddles::signaling::{production_native_join_capability, NativeJoinCapability};
use crate::huddles::state::{HuddleCommand, HuddleEvent, HuddleFailure, HuddlePhase};
use crate::message_handoff::{
    MessageHandoffResolver, MessageRef, ProviderFailure, ResolvedMessageHandoff,
};
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation,
    SlackConversationUnreadSnapshot, SlackFile, SlackMessage, SlackUnreadState, SlackUser,
    SlackUserGroup, SlackUserStatus, StoredToken,
};
use crate::realtime::RealtimeStatus;
use crate::runtime_sync::{
    RuntimeSyncAdmissionOutcome, RuntimeSyncFailureKind, RuntimeSyncReceipt, RuntimeSyncScheduler,
    RuntimeSyncTerminalResult, RuntimeSyncWork,
};
use crate::services::conversation_history::{recent_history_preview, ConversationHistoryService};
use crate::slack::{
    DownloadedPreviewAsset, SlackApi, SlackError, SlackErrorCategory, SlackMessagePage,
    SlackUnreadSnapshot, SlackUnreadSnapshotRecord,
};
use crate::socket_mode::{self, SocketModeDisconnect, SocketModeEvent, SocketModeMessageKind};
use crate::store::{
    StoreBatchExecution, StoreError, StoreErrorCategory, WorkspaceBootstrap, WorkspaceStore,
};
use crate::sync_scheduler::{
    AdmissionRejectionReason, AdmissionToken, CancellationId, FreshnessPolicy, JobOutcome,
    RefreshClass, ReplacementClass, RetryPolicy, SchedulerConfig, SyncDurability, SyncJob,
    SyncPriority, SyncTargetKey, SyncTargetKind,
};
use crate::workspace_pipeline::{
    same_message_identity, ConversationMembershipSnapshot, ConversationRefresh,
    MessageAttentionEffect, MessageChange, MessageMutationKind, MutationOrigin, ReactionMutation,
    SnapshotEnvelope, StoreBatch, StoreChange, TimelineTarget, WorkspaceAttentionContext,
    WorkspaceBootstrapData, WorkspaceChange, WorkspaceCoordinator, WorkspaceEffect,
    WorkspaceMutation, WorkspacePatch, WorkspaceReduction, WorkspaceRevision,
};
use crate::workspace_state::WorkspaceLifecycleEvent;

const CHANNEL_HISTORY_PREFETCH_LIMIT: usize = 12;
const CONVERSATION_ENRICHMENT_LIMIT: usize = 30;
const MAX_UNREAD_REFRESH_PASSES: usize = 3;
const UNREAD_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(1);
const CONVERSATION_PATCH_BATCH_SIZE: usize = 20;
const NAVIGATION_TASK_CONCURRENCY: usize = 2;
const INTERACTIVE_TASK_CONCURRENCY: usize = 8;
const BACKGROUND_TASK_CONCURRENCY: usize = 3;
const IMAGE_TASK_CONCURRENCY: usize = 4;
const UPLOAD_TASK_CONCURRENCY: usize = 2;
const SYNC_TASK_ADMISSION_CAPACITY: usize = 64;
const SYNC_TASK_RUNNING_CAPACITY: usize = 8;
const SYNC_TASK_STARVATION_BOUND: u64 = 8;
const SOCKET_MODE_INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const SOCKET_MODE_MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const ATTACHMENT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const ATTACHMENT_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const ATTACHMENT_BASENAME_MAX_BYTES: usize = 180;

#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    LoadStoredToken,
    StartOAuth {
        client_id: String,
        debug_auth: bool,
    },
    StartBrowserSession {
        xoxc_token: String,
        xoxd_token: String,
        user_agent: Option<String>,
    },
    SignOut,
    Disconnect,
    RefreshConversations,
    UpdateAttentionPreferences(AttentionPreferences),
    DiscoverChannels,
    DiscoverConversations,
    JoinConversation {
        channel_id: String,
    },
    LeaveConversation {
        channel_id: String,
    },
    OpenDirectMessage {
        user_id: String,
    },
    OpenGroupDirectMessage {
        user_ids: Vec<String>,
    },
    CreateChannel {
        name: String,
        is_private: bool,
    },
    InviteToChannel {
        channel_id: String,
        user_ids: Vec<String>,
    },
    LoadHistory {
        channel_id: String,
    },
    LoadOlderHistory {
        channel_id: String,
        cursor: String,
    },
    LoadThread {
        channel_id: String,
        ts: String,
    },
    LoadOlderThread {
        channel_id: String,
        ts: String,
        cursor: String,
    },
    LoadMessageContext(SearchMessageLocation),
    SearchMessages {
        query: String,
    },
    LoadFiles,
    LoadFile {
        file_id: String,
        share_requested: bool,
    },
    LoadSavedItems,
    LoadUser {
        user_id: String,
    },
    LoadUserProfile {
        user_id: String,
    },
    LoadImageAsset {
        key: String,
        url: String,
    },
    LoadMedia {
        url: String,
        name: String,
    },
    DownloadAttachment {
        url: String,
        name: String,
    },
    ResolveMessagePermalink {
        channel_id: String,
        ts: String,
    },
    MarkConversationRead {
        channel_id: String,
        ts: String,
    },
    MarkThreadRead {
        channel_id: String,
        thread_ts: String,
        ts: String,
    },
    PostMessage {
        channel_id: String,
        text: String,
        thread_ts: Option<String>,
    },
    SetReaction {
        channel_id: String,
        ts: String,
        name: String,
        add: bool,
        thread_ts: Option<String>,
    },
    SetSaved {
        channel_id: String,
        ts: String,
        add: bool,
        thread_ts: Option<String>,
    },
    SetConversationStarred {
        channel_id: String,
        starred: bool,
    },
    SetCurrentUserStatus {
        status: SlackUserStatus,
    },
    UploadFile {
        channel_id: String,
        thread_ts: Option<String>,
        path: PathBuf,
        initial_comment: Option<String>,
        remove_after_upload: bool,
    },
    Huddle(HuddleCommand),
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

impl SessionId {
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(u64);

impl RequestId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeIdentity {
    pub session: SessionId,
    pub request: RequestId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeOperation {
    Startup,
    Authenticate,
    SignOut,
    Disconnect,
    Conversations,
    ConversationDiscovery,
    OpenConversation,
    LeaveConversation,
    History,
    OlderHistory,
    Thread,
    OlderThread,
    Search,
    Files,
    SavedItems,
    User,
    Emoji,
    ReadMarker,
    ImageAsset,
    Media,
    AttachmentDownload,
    MessagePermalink,
    PostMessage,
    Reaction,
    Saved,
    ConversationStar,
    UserStatus,
    FileUpload,
    SocketMode,
    Huddle,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeTarget {
    Workspace,
    Channel(String),
    Thread {
        channel_id: String,
        thread_ts: String,
    },
    User(String),
    File(String),
    Image(String),
    Media(String),
    Attachment(String),
    ExactMessage {
        channel_id: String,
        ts: String,
    },
    Message {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Upload {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Huddle(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OperationContext {
    pub operation: RuntimeOperation,
    pub target: RuntimeTarget,
}

#[derive(Debug, Eq, PartialEq)]
struct RuntimeTraceFields {
    session: SessionId,
    request: RequestId,
    operation: RuntimeOperation,
    target: String,
}

impl RuntimeTraceFields {
    fn for_command(identity: RuntimeIdentity, command: &RuntimeCommand) -> Self {
        let context = command.operation_context();
        Self {
            session: identity.session,
            request: identity.request,
            operation: context.operation,
            target: runtime_target_for_trace(&context.target),
        }
    }

    fn span(&self) -> tracing::Span {
        tracing::debug_span!(
            target: "conduit::runtime",
            "runtime.command",
            session = ?self.session,
            request = ?self.request,
            operation = ?self.operation,
            target = %self.target,
        )
    }
}

fn runtime_target_for_trace(target: &RuntimeTarget) -> String {
    match target {
        RuntimeTarget::Workspace => "workspace".to_string(),
        RuntimeTarget::Channel(channel_id) => format!("channel:{channel_id}"),
        RuntimeTarget::Thread {
            channel_id,
            thread_ts,
        } => format!("thread:{channel_id}:{thread_ts}"),
        RuntimeTarget::User(user_id) => format!("user:{user_id}"),
        RuntimeTarget::File(file_id) => format!("file:{file_id}"),
        RuntimeTarget::Image(key) => format!("image:{}", crate::debug::url_for_log(key)),
        RuntimeTarget::Media(url) => format!("media:{}", crate::debug::url_for_log(url)),
        RuntimeTarget::Attachment(url) => {
            format!("attachment:{}", crate::debug::url_for_log(url))
        }
        RuntimeTarget::ExactMessage { channel_id, ts } => {
            format!("exact-message:{channel_id}:{ts}")
        }
        RuntimeTarget::Message {
            channel_id,
            thread_ts,
        } => format!(
            "message:{channel_id}:{}",
            thread_ts.as_deref().unwrap_or("main")
        ),
        RuntimeTarget::Upload {
            channel_id,
            thread_ts,
        } => format!(
            "upload:{channel_id}:{}",
            thread_ts.as_deref().unwrap_or("main")
        ),
        RuntimeTarget::Huddle(call_id) => format!("huddle:{call_id}"),
    }
}

impl OperationContext {
    pub fn new(operation: RuntimeOperation, target: RuntimeTarget) -> Self {
        Self { operation, target }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeCommandDescriptor {
    context: OperationContext,
    supersedes_previous: bool,
    navigation_slot: Option<NavigationSlot>,
    lane: RuntimeTaskLane,
}

impl RuntimeCommandDescriptor {
    fn request(context: OperationContext, lane: RuntimeTaskLane) -> Self {
        Self {
            context,
            supersedes_previous: true,
            navigation_slot: None,
            lane,
        }
    }

    fn navigation(context: OperationContext, slot: NavigationSlot) -> Self {
        Self {
            context,
            supersedes_previous: true,
            navigation_slot: Some(slot),
            lane: RuntimeTaskLane::Navigation,
        }
    }

    fn mutation(context: OperationContext, lane: RuntimeTaskLane) -> Self {
        Self {
            context,
            supersedes_previous: false,
            navigation_slot: None,
            lane,
        }
    }
}

impl RuntimeCommand {
    fn descriptor(&self) -> RuntimeCommandDescriptor {
        let workspace = |operation| OperationContext::new(operation, RuntimeTarget::Workspace);
        let channel = |operation, channel_id: &str| {
            OperationContext::new(operation, RuntimeTarget::Channel(channel_id.to_string()))
        };
        let thread = |operation, channel_id: &str, thread_ts: &str| {
            OperationContext::new(
                operation,
                RuntimeTarget::Thread {
                    channel_id: channel_id.to_string(),
                    thread_ts: thread_ts.to_string(),
                },
            )
        };

        match self {
            Self::LoadStoredToken => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::Startup),
                RuntimeTaskLane::Interactive,
            ),
            Self::StartOAuth { .. } | Self::StartBrowserSession { .. } => {
                RuntimeCommandDescriptor::request(
                    workspace(RuntimeOperation::Authenticate),
                    RuntimeTaskLane::Interactive,
                )
            }
            Self::SignOut => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::SignOut),
                RuntimeTaskLane::Interactive,
            ),
            Self::Disconnect => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::Disconnect),
                RuntimeTaskLane::Interactive,
            ),
            Self::RefreshConversations => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::Conversations),
                RuntimeTaskLane::Background,
            ),
            Self::UpdateAttentionPreferences(_) => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::SocketMode),
                RuntimeTaskLane::Interactive,
            ),
            Self::DiscoverConversations => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::ConversationDiscovery),
                RuntimeTaskLane::Background,
            ),
            Self::DiscoverChannels => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::ConversationDiscovery),
                RuntimeTaskLane::Background,
            ),
            Self::JoinConversation { channel_id } => RuntimeCommandDescriptor::request(
                channel(RuntimeOperation::OpenConversation, channel_id),
                RuntimeTaskLane::Interactive,
            ),
            Self::LeaveConversation { channel_id } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::LeaveConversation, channel_id),
                RuntimeTaskLane::Interactive,
            ),
            Self::OpenDirectMessage { user_id } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::User(user_id.clone()),
                ),
                RuntimeTaskLane::Interactive,
            ),
            Self::OpenGroupDirectMessage { .. } | Self::CreateChannel { .. } => {
                RuntimeCommandDescriptor::mutation(
                    workspace(RuntimeOperation::OpenConversation),
                    RuntimeTaskLane::Interactive,
                )
            }
            Self::InviteToChannel { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::OpenConversation, channel_id),
                RuntimeTaskLane::Interactive,
            ),
            Self::LoadHistory { channel_id } => RuntimeCommandDescriptor::navigation(
                channel(RuntimeOperation::History, channel_id),
                NavigationSlot::Main,
            ),
            Self::LoadOlderHistory { channel_id, .. } => RuntimeCommandDescriptor::navigation(
                channel(RuntimeOperation::OlderHistory, channel_id),
                NavigationSlot::Main,
            ),
            Self::LoadThread { channel_id, ts } => RuntimeCommandDescriptor::navigation(
                thread(RuntimeOperation::Thread, channel_id, ts),
                NavigationSlot::Thread,
            ),
            Self::LoadOlderThread { channel_id, ts, .. } => RuntimeCommandDescriptor::navigation(
                thread(RuntimeOperation::OlderThread, channel_id, ts),
                NavigationSlot::Thread,
            ),
            Self::LoadMessageContext(location) => RuntimeCommandDescriptor::navigation(
                message_context_operation_context(location),
                if location.thread_ts().is_some() {
                    NavigationSlot::Thread
                } else {
                    NavigationSlot::Main
                },
            ),
            Self::SearchMessages { .. } => RuntimeCommandDescriptor::navigation(
                workspace(RuntimeOperation::Search),
                NavigationSlot::Main,
            ),
            Self::LoadFiles => RuntimeCommandDescriptor::navigation(
                workspace(RuntimeOperation::Files),
                NavigationSlot::Main,
            ),
            Self::LoadFile { file_id, .. } => RuntimeCommandDescriptor::navigation(
                OperationContext::new(
                    RuntimeOperation::Files,
                    RuntimeTarget::File(file_id.clone()),
                ),
                NavigationSlot::Main,
            ),
            Self::LoadSavedItems => RuntimeCommandDescriptor::navigation(
                workspace(RuntimeOperation::SavedItems),
                NavigationSlot::Main,
            ),
            Self::LoadUser { user_id } | Self::LoadUserProfile { user_id } => {
                RuntimeCommandDescriptor::request(
                    OperationContext::new(
                        RuntimeOperation::User,
                        RuntimeTarget::User(user_id.clone()),
                    ),
                    RuntimeTaskLane::Background,
                )
            }
            Self::LoadImageAsset { key, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::ImageAsset,
                    RuntimeTarget::Image(key.clone()),
                ),
                RuntimeTaskLane::Image,
            ),
            Self::LoadMedia { url, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(RuntimeOperation::Media, RuntimeTarget::Media(url.clone())),
                RuntimeTaskLane::Image,
            ),
            Self::DownloadAttachment { url, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::AttachmentDownload,
                    RuntimeTarget::Attachment(url.clone()),
                ),
                RuntimeTaskLane::Image,
            ),
            Self::ResolveMessagePermalink { channel_id, ts } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::MessagePermalink,
                    RuntimeTarget::ExactMessage {
                        channel_id: channel_id.clone(),
                        ts: ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
            ),
            Self::MarkConversationRead { channel_id, .. } => RuntimeCommandDescriptor::request(
                channel(RuntimeOperation::ReadMarker, channel_id),
                RuntimeTaskLane::Interactive,
            ),
            Self::MarkThreadRead {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                thread(RuntimeOperation::ReadMarker, channel_id, thread_ts),
                RuntimeTaskLane::Interactive,
            ),
            Self::PostMessage {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::PostMessage,
                    RuntimeTarget::Message {
                        channel_id: channel_id.clone(),
                        thread_ts: thread_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
            ),
            Self::SetReaction {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::Reaction,
                    RuntimeTarget::Message {
                        channel_id: channel_id.clone(),
                        thread_ts: thread_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
            ),
            Self::SetSaved {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::Saved,
                    RuntimeTarget::Message {
                        channel_id: channel_id.clone(),
                        thread_ts: thread_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
            ),
            Self::SetConversationStarred { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::ConversationStar, channel_id),
                RuntimeTaskLane::Interactive,
            ),
            Self::SetCurrentUserStatus { .. } => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::UserStatus),
                RuntimeTaskLane::Interactive,
            ),
            Self::UploadFile {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::FileUpload,
                    RuntimeTarget::Upload {
                        channel_id: channel_id.clone(),
                        thread_ts: thread_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Upload,
            ),
            Self::Huddle(command) => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::Huddle,
                    RuntimeTarget::Huddle(command.call_id().unwrap_or("active").to_string()),
                ),
                RuntimeTaskLane::Interactive,
            ),
        }
    }

    pub fn supersedes_previous(&self) -> bool {
        self.descriptor().supersedes_previous
    }

    fn navigation_slot(&self) -> Option<NavigationSlot> {
        self.descriptor().navigation_slot
    }

    fn task_lane(&self) -> RuntimeTaskLane {
        self.descriptor().lane
    }

    pub fn operation_context(&self) -> OperationContext {
        self.descriptor().context
    }
}

fn message_context_operation_context(location: &SearchMessageLocation) -> OperationContext {
    location.thread_ts().map_or_else(
        || {
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel(location.channel_id().to_string()),
            )
        },
        |thread_ts| {
            OperationContext::new(
                RuntimeOperation::Thread,
                RuntimeTarget::Thread {
                    channel_id: location.channel_id().to_string(),
                    thread_ts: thread_ts.to_string(),
                },
            )
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFailureCategory {
    Authentication,
    Network,
    RateLimited,
    Storage,
    Validation,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFailure {
    pub category: RuntimeFailureCategory,
    pub message: String,
}

impl RuntimeFailure {
    pub fn from_error(error: &anyhow::Error) -> Self {
        for source in error.chain() {
            if let Some(slack) = source.downcast_ref::<crate::slack::SlackError>() {
                if slack.is_permission_denied() {
                    return Self::validation(
                        "Slack does not allow this action for this conversation.",
                    );
                }
                return Self::from_slack_category(slack.category());
            }
            if let Some(store) = source.downcast_ref::<crate::store::StoreError>() {
                return Self::from_store_category(store.category());
            }
        }
        Self::internal()
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            category: RuntimeFailureCategory::Validation,
            message: message.into(),
        }
    }

    fn internal() -> Self {
        Self {
            category: RuntimeFailureCategory::Internal,
            message: "Conduit encountered an unexpected error.".to_string(),
        }
    }

    fn from_slack_category(category: SlackErrorCategory) -> Self {
        match category {
            SlackErrorCategory::Authentication => Self {
                category: RuntimeFailureCategory::Authentication,
                message: "Slack authentication failed. Sign in again.".to_string(),
            },
            SlackErrorCategory::Connectivity => Self {
                category: RuntimeFailureCategory::Network,
                message: "Could not reach Slack. Check your connection and try again.".to_string(),
            },
            SlackErrorCategory::RateLimited => Self {
                category: RuntimeFailureCategory::RateLimited,
                message: "Slack is rate limiting requests. Try again shortly.".to_string(),
            },
            SlackErrorCategory::LocalIo => Self::storage(),
            SlackErrorCategory::Validation => Self {
                category: RuntimeFailureCategory::Validation,
                message: "Slack rejected invalid input.".to_string(),
            },
            SlackErrorCategory::Unexpected => Self::internal(),
        }
    }

    fn from_store_category(category: StoreErrorCategory) -> Self {
        match category {
            StoreErrorCategory::RejectedUpdate => Self::internal(),
            StoreErrorCategory::LocalIo
            | StoreErrorCategory::TemporarilyUnavailable
            | StoreErrorCategory::CorruptData
            | StoreErrorCategory::IncompatibleSchema => Self::storage(),
            StoreErrorCategory::Unexpected => Self::internal(),
        }
    }

    fn storage() -> Self {
        Self {
            category: RuntimeFailureCategory::Storage,
            message: "Conduit could not access its local data.".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthenticationFailureContext {
    Default,
    BrowserSession,
}

fn authentication_failure(
    context: AuthenticationFailureContext,
    error: &anyhow::Error,
) -> RuntimeFailure {
    let failure = RuntimeFailure::from_error(error);
    if context != AuthenticationFailureContext::BrowserSession {
        return failure;
    }

    match failure.category {
        RuntimeFailureCategory::Authentication => RuntimeFailure {
            category: failure.category,
            message: "Slack rejected the XOXC/XOXD browser session. Recopy both values from the same signed-in browser and try again."
                .to_string(),
        },
        RuntimeFailureCategory::Network => RuntimeFailure {
            category: failure.category,
            message: "Could not validate XOXC/XOXD. Check the connection and, for Enterprise Slack, paste the exact User-Agent from the same browser. If that still fails, use OAuth because Conduit cannot reproduce the browser's TLS fingerprint."
                .to_string(),
        },
        _ => failure,
    }
}

#[derive(Debug)]
pub enum RuntimeEventKind {
    Status(String),
    WorkspaceLifecycle(WorkspaceLifecycleEvent),
    Error(RuntimeFailure),
    RuntimeStartFailed(RuntimeFailure),
    SignedOut,
    Authenticated(AuthInfo),
    WorkspacePatch(WorkspacePatch),
    ConversationsSynchronized,
    ConversationsLoadFailed(RuntimeFailure),
    ConversationChannelsDiscovered(Vec<SlackConversation>),
    ConversationPeopleDiscovered(Vec<SlackUser>),
    ConversationOpened {
        channel_id: String,
    },
    ConversationUpdated {
        channel_id: String,
    },
    ConversationStarUpdated {
        channel_id: String,
        starred: bool,
    },
    CurrentUserStatusUpdated {
        user_id: String,
        status: Option<SlackUserStatus>,
    },
    ConversationLeft {
        channel_id: String,
    },
    AttentionNotificationCandidate {
        channel_id: String,
        message: Box<SlackMessage>,
        decision: AttentionDecision,
    },
    HistoryLoaded {
        channel_id: String,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
        cached: bool,
    },
    ThreadLoaded {
        channel_id: String,
        ts: String,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
    },
    MessageContextLoaded {
        location: SearchMessageLocation,
        messages: Vec<SlackMessage>,
    },
    SearchLoaded(Vec<SearchMatch>),
    FilesLoaded(Vec<SlackFile>),
    FileLoaded {
        file: Box<SlackFile>,
        share_requested: bool,
    },
    SavedItemsLoaded(Vec<SavedItem>),
    UserLoaded {
        user_id: String,
        display_name: String,
        full_name: Option<String>,
        avatar_url: Option<String>,
        status: Option<SlackUserStatus>,
    },
    UserProfileLoaded(Box<SlackUser>),
    UserNamesLoaded(HashMap<String, String>),
    UserFullNamesLoaded(HashMap<String, String>),
    UserAvatarUrlsLoaded(HashMap<String, String>),
    UserSearchAliasesLoaded(HashMap<String, Vec<String>>),
    UserStatusesLoaded {
        statuses: HashMap<String, SlackUserStatus>,
        replace_existing: bool,
        preserve_user_ids: HashSet<String>,
    },
    UserGroupsLoaded {
        names: HashMap<String, String>,
        members: HashMap<String, Vec<String>>,
    },
    EmojiCatalogLoaded(HashMap<String, String>),
    ImageAssetLoaded {
        key: String,
        data_uri: String,
    },
    ImageAssetFailed {
        key: String,
    },
    MediaLoaded {
        url: String,
        name: String,
        path: PathBuf,
        mime_type: String,
    },
    AttachmentDownloadProgress {
        fraction: f64,
        label: String,
    },
    AttachmentDownloaded {
        url: String,
        name: String,
        path: PathBuf,
    },
    MessagePermalinkResolved {
        handoff: ResolvedMessageHandoff,
    },
    MessagePosted {
        channel_id: String,
        message: Box<SlackMessage>,
    },
    ReactionUpdated {
        channel_id: String,
        ts: String,
        name: String,
        added: bool,
        thread_ts: Option<String>,
    },
    SavedUpdated {
        channel_id: String,
        saved: bool,
        thread_ts: Option<String>,
    },
    RealtimeStatusChanged(RealtimeStatus),
    SocketModeEvent {
        event: SocketModeEvent,
        attention: Option<AttentionDecision>,
    },
    Huddle(HuddleEvent),
    FileUploadProgress {
        fraction: f64,
        label: String,
    },
    FileUploaded(String),
}

impl RuntimeEventKind {
    pub fn operation_context(&self, fallback: &OperationContext) -> OperationContext {
        match self {
            Self::SignedOut => {
                OperationContext::new(RuntimeOperation::SignOut, RuntimeTarget::Workspace)
            }
            Self::Authenticated(_) => {
                OperationContext::new(RuntimeOperation::Authenticate, RuntimeTarget::Workspace)
            }
            Self::WorkspacePatch(_)
            | Self::ConversationsSynchronized
            | Self::ConversationsLoadFailed(_) => {
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
            }
            Self::AttentionNotificationCandidate { .. } => {
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace)
            }
            Self::ConversationChannelsDiscovered(_) | Self::ConversationPeopleDiscovered(_) => {
                OperationContext::new(
                    RuntimeOperation::ConversationDiscovery,
                    RuntimeTarget::Workspace,
                )
            }
            Self::ConversationOpened { channel_id } | Self::ConversationUpdated { channel_id } => {
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel(channel_id.clone()),
                )
            }
            Self::ConversationStarUpdated { channel_id, .. } => OperationContext::new(
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::CurrentUserStatusUpdated { .. } => {
                OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace)
            }
            Self::ConversationLeft { channel_id } => OperationContext::new(
                RuntimeOperation::LeaveConversation,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::HistoryLoaded {
                channel_id,
                append_older,
                ..
            } => OperationContext::new(
                if *append_older {
                    RuntimeOperation::OlderHistory
                } else {
                    RuntimeOperation::History
                },
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::ThreadLoaded {
                channel_id,
                ts,
                append_older,
                ..
            } => OperationContext::new(
                if *append_older {
                    RuntimeOperation::OlderThread
                } else {
                    RuntimeOperation::Thread
                },
                RuntimeTarget::Thread {
                    channel_id: channel_id.clone(),
                    thread_ts: ts.clone(),
                },
            ),
            Self::MessageContextLoaded { location, .. } => {
                message_context_operation_context(location)
            }
            Self::SearchLoaded(_) => {
                OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace)
            }
            Self::FilesLoaded(_) => {
                OperationContext::new(RuntimeOperation::Files, RuntimeTarget::Workspace)
            }
            Self::FileLoaded { file, .. } => OperationContext::new(
                RuntimeOperation::Files,
                RuntimeTarget::File(file.id.clone().unwrap_or_default()),
            ),
            Self::SavedItemsLoaded(_) => {
                OperationContext::new(RuntimeOperation::SavedItems, RuntimeTarget::Workspace)
            }
            Self::UserLoaded { user_id, .. } => {
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::User(user_id.clone()))
            }
            Self::UserProfileLoaded(user) => OperationContext::new(
                RuntimeOperation::User,
                RuntimeTarget::User(user.id.clone().unwrap_or_default()),
            ),
            Self::UserNamesLoaded(_)
            | Self::UserFullNamesLoaded(_)
            | Self::UserAvatarUrlsLoaded(_)
            | Self::UserSearchAliasesLoaded(_)
            | Self::UserStatusesLoaded { .. }
            | Self::UserGroupsLoaded { .. } => {
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::Workspace)
            }
            Self::EmojiCatalogLoaded(_) => {
                OperationContext::new(RuntimeOperation::Emoji, RuntimeTarget::Workspace)
            }
            Self::ImageAssetLoaded { key, .. } | Self::ImageAssetFailed { key } => {
                OperationContext::new(
                    RuntimeOperation::ImageAsset,
                    RuntimeTarget::Image(key.clone()),
                )
            }
            Self::MediaLoaded { url, .. } => {
                OperationContext::new(RuntimeOperation::Media, RuntimeTarget::Media(url.clone()))
            }
            Self::AttachmentDownloaded { url, .. } => OperationContext::new(
                RuntimeOperation::AttachmentDownload,
                RuntimeTarget::Attachment(url.clone()),
            ),
            Self::MessagePermalinkResolved { handoff } => OperationContext::new(
                RuntimeOperation::MessagePermalink,
                RuntimeTarget::ExactMessage {
                    channel_id: handoff.target.channel_id().to_string(),
                    ts: handoff.target.timestamp().to_string(),
                },
            ),
            Self::RealtimeStatusChanged(_) | Self::SocketModeEvent { .. } => {
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace)
            }
            Self::Huddle(event) => OperationContext::new(
                RuntimeOperation::Huddle,
                RuntimeTarget::Huddle(event.call_id().unwrap_or("active").to_string()),
            ),
            Self::RuntimeStartFailed(_) => {
                OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace)
            }
            Self::WorkspaceLifecycle(_) => {
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
            }
            Self::Status(_)
            | Self::Error(_)
            | Self::MessagePosted { .. }
            | Self::ReactionUpdated { .. }
            | Self::SavedUpdated { .. }
            | Self::AttachmentDownloadProgress { .. }
            | Self::FileUploadProgress { .. }
            | Self::FileUploaded(_) => fallback.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeEventMeta {
    pub session: SessionId,
    pub request: Option<RequestId>,
    pub context: OperationContext,
}

impl RuntimeEventMeta {
    pub fn new(identity: RuntimeIdentity, context: OperationContext) -> Self {
        Self {
            session: identity.session,
            request: Some(identity.request),
            context,
        }
    }
}

#[derive(Debug)]
pub struct RuntimeEvent {
    pub meta: RuntimeEventMeta,
    pub kind: RuntimeEventKind,
}

#[derive(Debug)]
struct RuntimeRequest {
    identity: RuntimeIdentity,
    command: RuntimeCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTaskLane {
    Navigation,
    Interactive,
    Background,
    Image,
    Upload,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum NavigationSlot {
    Main,
    Thread,
}

#[derive(Clone, Debug)]
struct RuntimeTaskLimits {
    navigation: Arc<Semaphore>,
    interactive: Arc<Semaphore>,
    background: Arc<Semaphore>,
    image: Arc<Semaphore>,
    upload: Arc<Semaphore>,
}

impl RuntimeTaskLimits {
    fn new(
        navigation: usize,
        interactive: usize,
        background: usize,
        image: usize,
        upload: usize,
    ) -> Self {
        Self {
            navigation: Arc::new(Semaphore::new(navigation)),
            interactive: Arc::new(Semaphore::new(interactive)),
            background: Arc::new(Semaphore::new(background)),
            image: Arc::new(Semaphore::new(image)),
            upload: Arc::new(Semaphore::new(upload)),
        }
    }

    async fn acquire(&self, lane: RuntimeTaskLane) -> OwnedSemaphorePermit {
        let semaphore = match lane {
            RuntimeTaskLane::Navigation => Arc::clone(&self.navigation),
            RuntimeTaskLane::Interactive => Arc::clone(&self.interactive),
            RuntimeTaskLane::Background => Arc::clone(&self.background),
            RuntimeTaskLane::Image => Arc::clone(&self.image),
            RuntimeTaskLane::Upload => Arc::clone(&self.upload),
        };
        semaphore
            .acquire_owned()
            .await
            .expect("runtime task semaphore unexpectedly closed")
    }
}

#[derive(Clone)]
struct RuntimeConnection {
    slack: SlackApi,
    workspace_url: Option<String>,
    workspace_store: Option<WorkspaceStore>,
    workspace: WorkspaceReducerAdapter,
    current_user_id: Option<String>,
    user_cache: Arc<Mutex<HashMap<String, String>>>,
    read_marks: Arc<Mutex<HashMap<String, String>>>,
    message_handoffs: Arc<Mutex<MessageHandoffResolver>>,
    conversation_star_sync: ConversationStarSyncGate,
    user_status_sync: UserStatusSync,
    team_id: Option<String>,
    huddles: HuddleActorHandle,
    #[cfg(test)]
    cached_bootstrap_load_gate: Option<Arc<TestWorkspacePatchSendGate>>,
}

type ConversationStarSyncGate = Arc<tokio::sync::Mutex<()>>;

#[derive(Clone, Debug, Default)]
struct UserStatusSync {
    state: Arc<Mutex<UserStatusSyncState>>,
    persistence: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Default)]
struct UserStatusSyncState {
    revision: u64,
    user_revisions: HashMap<String, u64>,
}

impl UserStatusSync {
    fn revision(&self) -> u64 {
        self.state
            .lock()
            .expect("user status sync lock poisoned")
            .revision
    }

    fn user_revision(&self, user_id: &str) -> u64 {
        self.state
            .lock()
            .expect("user status sync lock poisoned")
            .user_revisions
            .get(user_id)
            .copied()
            .unwrap_or_default()
    }

    fn is_revision_current(&self, revision: u64) -> bool {
        self.revision() == revision
    }

    fn is_user_revision_current(&self, user_id: &str, revision: u64) -> bool {
        self.user_revision(user_id) == revision
    }

    fn publish_change(&self, user_id: &str, publish: impl FnOnce()) -> u64 {
        let mut state = self.state.lock().expect("user status sync lock poisoned");
        state.revision = state.revision.saturating_add(1);
        let revision = state.revision;
        state.user_revisions.insert(user_id.to_string(), revision);
        publish();
        revision
    }

    fn publish_snapshot(&self, base_revision: u64, publish: impl FnOnce(HashSet<String>)) {
        let state = self.state.lock().expect("user status sync lock poisoned");
        let preserve_user_ids = state
            .user_revisions
            .iter()
            .filter_map(|(user_id, revision)| {
                (*revision > base_revision).then_some(user_id.clone())
            })
            .collect();
        publish(preserve_user_ids);
    }

    fn publish_user_snapshot(&self, user_id: &str, base_revision: u64, publish: impl FnOnce(bool)) {
        let state = self.state.lock().expect("user status sync lock poisoned");
        let is_current = state
            .user_revisions
            .get(user_id)
            .copied()
            .unwrap_or_default()
            == base_revision;
        publish(is_current);
    }
}

#[derive(Debug)]
struct ThreadPagingSession {
    channel_id: String,
    thread_ts: String,
    base_revision: WorkspaceRevision,
    web_api_messages: Vec<SlackMessage>,
    local_messages: Vec<SlackMessage>,
    local_tombstones: HashSet<String>,
}

impl ThreadPagingSession {
    fn matches(&self, channel_id: &str, thread_ts: &str) -> bool {
        self.channel_id == channel_id && self.thread_ts == thread_ts
    }

    fn record_web_api_page(&mut self, messages: impl IntoIterator<Item = SlackMessage>) {
        for message in messages {
            upsert_thread_message(&mut self.web_api_messages, message);
        }
    }

    fn record_local_changes(&mut self, changes: &[MessageChange]) {
        for change in changes {
            match change {
                MessageChange::Upsert(message) => {
                    self.local_tombstones.remove(&message.ts);
                    upsert_thread_message(&mut self.local_messages, (**message).clone());
                }
                MessageChange::Remove { message_ts } => {
                    self.local_messages
                        .retain(|message| message.ts != *message_ts);
                    self.local_tombstones.insert(message_ts.clone());
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct ThreadPagingAccumulator {
    active: Option<ThreadPagingSession>,
}

impl ThreadPagingAccumulator {
    fn clear(&mut self) {
        self.active = None;
    }

    fn begin(&mut self, channel_id: &str, thread_ts: &str, base_revision: WorkspaceRevision) {
        self.active = Some(ThreadPagingSession {
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
            base_revision,
            web_api_messages: Vec::new(),
            local_messages: Vec::new(),
            local_tombstones: HashSet::new(),
        });
    }

    fn clear_matching(&mut self, channel_id: &str, thread_ts: &str) {
        if self
            .active
            .as_ref()
            .is_some_and(|session| session.matches(channel_id, thread_ts))
        {
            self.clear();
        }
    }

    fn record_web_api_page(
        &mut self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
        complete: bool,
    ) {
        let Some(session) = self
            .active
            .as_mut()
            .filter(|session| session.matches(channel_id, thread_ts))
        else {
            return;
        };
        session.record_web_api_page(messages.iter().cloned());
        if complete {
            self.clear();
        }
    }

    fn record_local_patch(&mut self, patch: &WorkspacePatch) {
        let Some(session) = self.active.as_mut() else {
            return;
        };
        for change in patch.changes() {
            let WorkspaceChange::TimelineChanged { target, changes } = change else {
                continue;
            };
            let TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } = target
            else {
                continue;
            };
            if session.matches(channel_id, thread_ts) {
                session.record_local_changes(changes);
            }
        }
    }

    fn older_page(
        &mut self,
        channel_id: &str,
        thread_ts: &str,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        fallback_base_revision: WorkspaceRevision,
    ) -> SnapshotEnvelope<crate::workspace_pipeline::MessagePage> {
        let complete = thread_page_is_complete(has_more, next_cursor.as_deref());
        let session_base_revision = self
            .active
            .as_ref()
            .filter(|session| session.matches(channel_id, thread_ts))
            .map(|session| session.base_revision);
        let base_revision = session_base_revision.unwrap_or(fallback_base_revision);
        if !complete {
            if session_base_revision.is_some() {
                self.active
                    .as_mut()
                    .expect("matching thread paging session disappeared")
                    .record_web_api_page(messages.iter().cloned());
            }
            return SnapshotEnvelope::new(
                base_revision,
                crate::workspace_pipeline::MessagePage {
                    messages,
                    next_cursor,
                    complete: false,
                },
            );
        }
        if session_base_revision.is_none() {
            return SnapshotEnvelope::new(
                base_revision,
                crate::workspace_pipeline::MessagePage {
                    messages,
                    next_cursor,
                    complete: false,
                },
            );
        }

        let mut session = self
            .active
            .take()
            .expect("matching thread paging session disappeared");
        session.record_web_api_page(messages);
        SnapshotEnvelope::new(
            base_revision,
            crate::workspace_pipeline::MessagePage {
                messages: complete_thread_messages(session),
                next_cursor,
                complete: true,
            },
        )
    }
}

fn complete_thread_messages(session: ThreadPagingSession) -> Vec<SlackMessage> {
    let mut messages = session
        .web_api_messages
        .into_iter()
        .filter(|message| !session.local_tombstones.contains(&message.ts))
        .collect::<Vec<_>>();
    for message in session.local_messages {
        upsert_thread_message(&mut messages, message);
    }
    messages.sort_by(|left, right| left.ts.cmp(&right.ts));
    messages
}

fn upsert_thread_message(messages: &mut Vec<SlackMessage>, message: SlackMessage) {
    messages.retain(|known| !same_message_identity(known, &message));
    messages.push(message);
}

fn thread_page_is_complete(has_more: bool, next_cursor: Option<&str>) -> bool {
    !has_more && next_cursor.is_none_or(|cursor| cursor.trim().is_empty())
}

#[derive(Clone, Debug, Default)]
struct WorkspaceReducerAdapter {
    coordinator: Arc<Mutex<WorkspaceCoordinator>>,
    attention_metrics: Arc<AttentionMetrics>,
    publication_admission: Arc<tokio::sync::Mutex<()>>,
    pending_writes: Arc<Mutex<std::collections::VecDeque<PendingWorkspaceWrite>>>,
    thread_paging: Arc<Mutex<ThreadPagingAccumulator>>,
    #[cfg(test)]
    history_completion_send_gate: Arc<Mutex<Option<Arc<TestWorkspacePatchSendGate>>>>,
    #[cfg(test)]
    workspace_repair_ack_gate: Arc<Mutex<Option<Arc<TestWorkspaceRepairAckGate>>>>,
}

#[derive(Clone, Debug)]
struct PendingWorkspaceWrite {
    batch: Option<StoreBatch>,
    reduction: Option<WorkspaceReduction>,
    persisted: bool,
    notification_claimed: bool,
}

#[derive(Clone, Debug)]
struct PersistedWorkspaceWrite {
    reduction: WorkspaceReduction,
    notification_claimed: bool,
}

/// Keeps the cache reset gate exclusive until every recovered event is sent.
struct PersistedWorkspacePublication {
    writes: Vec<PersistedWorkspaceWrite>,
    _recovery_publication: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
}

impl PersistedWorkspacePublication {
    fn writes(&self) -> &[PersistedWorkspaceWrite] {
        &self.writes
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    fn into_reductions(self) -> Vec<WorkspaceReduction> {
        self.writes
            .into_iter()
            .map(|write| write.reduction)
            .collect()
    }

    #[cfg(test)]
    fn into_writes(self) -> Vec<PersistedWorkspaceWrite> {
        self.writes
    }
}

impl PersistedWorkspaceWrite {
    fn notification(&self) -> Option<MessageAttentionEffect> {
        let applied_attention = self
            .reduction
            .effects()
            .iter()
            .find_map(|effect| match effect {
                WorkspaceEffect::MessageAttention(effect) => Some(effect),
                WorkspaceEffect::ThreadRead(_) => None,
            });
        claimed_notification_candidate(self.notification_claimed, applied_attention)
    }
}

impl WorkspaceReducerAdapter {
    fn revision(&self) -> WorkspaceRevision {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .revision()
    }

    fn conversations(&self) -> Vec<SlackConversation> {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .conversations()
    }

    fn clear_thread_paging(&self) {
        self.thread_paging
            .lock()
            .expect("thread paging accumulator lock poisoned")
            .clear();
    }

    fn begin_thread_fetch(&self, channel_id: &str, thread_ts: &str) -> WorkspaceRevision {
        let coordinator = self
            .coordinator
            .lock()
            .expect("workspace coordinator lock poisoned");
        let base_revision = coordinator.revision();
        self.thread_paging
            .lock()
            .expect("thread paging accumulator lock poisoned")
            .begin(channel_id, thread_ts, base_revision);
        base_revision
    }

    fn clear_matching_thread_fetch(&self, channel_id: &str, thread_ts: &str) {
        self.thread_paging
            .lock()
            .expect("thread paging accumulator lock poisoned")
            .clear_matching(channel_id, thread_ts);
    }

    fn record_initial_thread_page(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
        complete: bool,
    ) {
        self.thread_paging
            .lock()
            .expect("thread paging accumulator lock poisoned")
            .record_web_api_page(channel_id, thread_ts, messages, complete);
    }

    fn older_thread_snapshot_page(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        fallback_base_revision: WorkspaceRevision,
    ) -> SnapshotEnvelope<crate::workspace_pipeline::MessagePage> {
        self.thread_paging
            .lock()
            .expect("thread paging accumulator lock poisoned")
            .older_page(
                channel_id,
                thread_ts,
                messages,
                has_more,
                next_cursor,
                fallback_base_revision,
            )
    }

    #[cfg(test)]
    fn history(&self, channel_id: &str) -> Vec<SlackMessage> {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .history(channel_id)
    }

    #[cfg(test)]
    fn set_history_completion_send_gate(&self, gate: Arc<TestWorkspacePatchSendGate>) {
        *self
            .history_completion_send_gate
            .lock()
            .expect("history completion gate lock poisoned") = Some(gate);
    }

    #[cfg(test)]
    fn wait_before_history_completion(&self) {
        let gate = self
            .history_completion_send_gate
            .lock()
            .expect("history completion gate lock poisoned")
            .clone();
        if let Some(gate) = gate {
            gate.wait_before_send();
        }
    }

    #[cfg(test)]
    fn set_workspace_repair_ack_gate(&self, gate: Arc<TestWorkspaceRepairAckGate>) {
        *self
            .workspace_repair_ack_gate
            .lock()
            .expect("workspace repair acknowledgment gate lock poisoned") = Some(gate);
    }

    #[cfg(test)]
    async fn wait_before_workspace_repair_ack(&self) {
        let gate = self
            .workspace_repair_ack_gate
            .lock()
            .expect("workspace repair acknowledgment gate lock poisoned")
            .take();
        if let Some(gate) = gate {
            gate.wait().await;
        }
    }

    fn apply(
        &self,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> Option<WorkspaceReduction> {
        let reduction = {
            let mut coordinator = self
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            let reduction = coordinator.apply_from(origin, mutation);
            if matches!(origin, MutationOrigin::Local | MutationOrigin::Realtime) {
                if let Some(reduction) = reduction.as_ref() {
                    self.thread_paging
                        .lock()
                        .expect("thread paging accumulator lock poisoned")
                        .record_local_patch(reduction.patch());
                }
            }
            reduction
        };
        if let Some(reduction) = reduction.as_ref() {
            let revision = reduction.patch().revision().value();
            for effect in reduction.effects() {
                match effect {
                    WorkspaceEffect::MessageAttention(effect) => {
                        self.attention_metrics.record_decision(
                            revision,
                            origin,
                            effect.delivery,
                            &effect.decision,
                        );
                    }
                    WorkspaceEffect::ThreadRead(_) => {}
                }
            }
            crate::debug::log(
                "workspace",
                &format!(
                    "WorkspaceMutationApplied origin={origin:?} revision={} patch_changes={} store_changes={} effects={}",
                    revision,
                    reduction.patch().changes().len(),
                    reduction
                        .store_batch()
                        .map_or(0, |batch| batch.changes().len()),
                    reduction.effects().len(),
                ),
            );
        }
        reduction
    }

    #[cfg(test)]
    async fn apply_persisted(
        &self,
        store: Option<&WorkspaceStore>,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        let _admission = self.publication_admission.lock().await;
        Ok(self
            .apply_persisted_admitted(store, origin, mutation)
            .await?
            .into_reductions())
    }

    /// Returns persisted reductions in revision order while the caller holds
    /// store admission. A pending failure does not prevent the current mutation
    /// from entering the ordered journal.
    async fn apply_persisted_admitted(
        &self,
        store: Option<&WorkspaceStore>,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> std::result::Result<PersistedWorkspacePublication, StoreError> {
        let pending_error = self.persist_pending_writes(store).await.err();

        self.apply_and_enqueue(store, origin, mutation);

        // An externally confirmed mutation must enter the coordinator journal
        // even while an older accepted batch remains temporarily unavailable.
        if let Some(error) = pending_error {
            return Err(error);
        }
        self.recover_persisted_admitted(store).await
    }

    /// Applies one mutation and appends its complete reduction to the ordered
    /// journal. The caller must hold store admission.
    fn apply_and_enqueue(
        &self,
        store: Option<&WorkspaceStore>,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> Option<WorkspaceReduction> {
        let reduction = self.apply(origin, mutation)?;
        let batch = reduction.store_batch().cloned();
        let persisted = store.is_none() || batch.is_none();
        self.pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned")
            .push_back(PendingWorkspaceWrite {
                batch,
                reduction: Some(reduction.clone()),
                persisted,
                notification_claimed: false,
            });
        Some(reduction)
    }

    /// Flushes and drains all admitted reductions in revision order. Socket
    /// messages use this before durable attention classification so an older
    /// read or unread mutation cannot make the classification stale.
    async fn recover_persisted_admitted(
        &self,
        store: Option<&WorkspaceStore>,
    ) -> std::result::Result<PersistedWorkspacePublication, StoreError> {
        loop {
            self.persist_pending_writes(store).await?;
            let Some(store) = store else {
                return Ok(PersistedWorkspacePublication {
                    writes: self.drain_persisted_admitted(),
                    _recovery_publication: None,
                });
            };
            let recovery_publication = store.lock_recovery_linearization().await;
            if store.workspace_cache_needs_repair() {
                continue;
            }
            return Ok(PersistedWorkspacePublication {
                writes: self.drain_persisted_admitted(),
                _recovery_publication: Some(recovery_publication),
            });
        }
    }

    /// Drains only after the caller has completed any awaited work that must
    /// precede publication.
    fn drain_persisted_admitted(&self) -> Vec<PersistedWorkspaceWrite> {
        let mut pending = self
            .pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned");
        debug_assert!(pending.iter().all(|entry| entry.persisted));
        pending
            .drain(..)
            .filter_map(|entry| {
                entry.reduction.map(|reduction| PersistedWorkspaceWrite {
                    reduction,
                    notification_claimed: entry.notification_claimed,
                })
            })
            .collect()
    }

    /// Delivers only reductions whose complete store batches are durable. Any
    /// recovered reductions are emitted first in their original revision order.
    async fn apply_persisted_and_publish(
        &self,
        store: Option<&WorkspaceStore>,
        events: &RuntimeEventSender,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        self.apply_persisted_and_publish_inner(store, events, origin, mutation, None)
            .await
    }

    async fn apply_persisted_and_publish_with_completion(
        &self,
        store: Option<&WorkspaceStore>,
        events: &RuntimeEventSender,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
        completion: RuntimeEventKind,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        self.apply_persisted_and_publish_inner(store, events, origin, mutation, Some(completion))
            .await
    }

    async fn apply_persisted_and_publish_inner(
        &self,
        store: Option<&WorkspaceStore>,
        events: &RuntimeEventSender,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
        completion: Option<RuntimeEventKind>,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        let _admission = self.publication_admission.lock().await;
        self.apply_persisted_and_publish_admitted(store, events, origin, mutation, completion)
            .await
    }

    /// Applies and publishes while the caller retains store admission across
    /// work that must precede the mutation, such as reading cache hydration.
    async fn apply_persisted_and_publish_admitted(
        &self,
        store: Option<&WorkspaceStore>,
        events: &RuntimeEventSender,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
        completion: Option<RuntimeEventKind>,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        let publication = self
            .apply_persisted_and_publish_retained_admitted(
                store, events, origin, mutation, completion,
            )
            .await?;
        Ok(publication.into_reductions())
    }

    async fn apply_persisted_and_publish_retained_admitted(
        &self,
        store: Option<&WorkspaceStore>,
        events: &RuntimeEventSender,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
        completion: Option<RuntimeEventKind>,
    ) -> std::result::Result<PersistedWorkspacePublication, StoreError> {
        let publication = self
            .apply_persisted_admitted(store, origin, mutation)
            .await?;
        for write in publication.writes() {
            publish_persisted_workspace_write(events, write);
        }
        if let Some(completion) = completion {
            events.send_event(completion);
        }
        Ok(publication)
    }

    async fn repair_workspace_cache_admitted(
        &self,
        store: &WorkspaceStore,
    ) -> std::result::Result<(), StoreError> {
        loop {
            let recovery_generation = store.recovery_generation();
            if !store
                .ensure_workspace_cache_reset_for_repair(recovery_generation)
                .await?
            {
                continue;
            }
            let (revision, projection) = {
                let coordinator = self
                    .coordinator
                    .lock()
                    .expect("workspace coordinator lock poisoned");
                (coordinator.revision(), coordinator.store_projection())
            };
            let replay_changes = self
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned")
                .iter()
                .filter_map(|entry| entry.batch.as_ref())
                .filter(|batch| batch.revision() <= revision)
                .flat_map(StoreBatch::workspace_repair_replay_changes)
                .collect::<Vec<_>>();
            let mut repair_changes = vec![StoreChange::WorkspaceRepaired(projection)];
            repair_changes.extend(replay_changes);
            if let Some(batch) = StoreBatch::new(revision, repair_changes) {
                let expected_claims = batch.notification_claims();
                let outcome = store.execute_store_repair_batch_with_claims(batch).await?;
                if !expected_claims.iter().all(|expected| {
                    outcome
                        .notification_claims
                        .iter()
                        .any(|claim| claim.identity == *expected)
                }) {
                    return Err(StoreError::rejected_update(
                        "notification delivery claim was not persisted or replayed",
                    ));
                }
                let mut pending = self
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned");
                for claim in outcome
                    .notification_claims
                    .iter()
                    .filter(|claim| claim.notification_claimed)
                {
                    if let Some(target) = pending.iter_mut().find(|entry| {
                        entry.reduction.is_some()
                            && entry.batch.as_ref().is_some_and(|batch| {
                                batch.revision() <= revision
                                    && batch
                                        .notification_claims()
                                        .iter()
                                        .any(|identity| identity == &claim.identity)
                            })
                    }) {
                        target.notification_claimed = true;
                    }
                }
                drop(pending);
                match outcome.execution {
                    StoreBatchExecution::Committed | StoreBatchExecution::Unchanged => {}
                    StoreBatchExecution::SkippedStale => {
                        return Err(StoreError::rejected_update(
                            "workspace cache repair revision was stale",
                        ));
                    }
                }
            }
            #[cfg(test)]
            self.wait_before_workspace_repair_ack().await;

            let _recovery = store.lock_recovery_linearization().await;
            let coordinator = self
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            if store.recovery_generation() != recovery_generation
                || store.workspace_cache_needs_reset()
                || coordinator.revision() != revision
            {
                continue;
            }

            // The exact current projection durably includes every older
            // projectable delta. Explicit projection-independent, idempotent
            // changes were replayed in journal order in the same transaction,
            // so retry cannot change their durable result. Keep each complete
            // journal entry replayable until the final recovery-locked drain.
            let mut pending = self
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned");
            for entry in pending.iter_mut().take_while(|entry| {
                entry
                    .reduction
                    .as_ref()
                    .map_or_else(
                        || entry.batch.as_ref().map(StoreBatch::revision),
                        |reduction| Some(reduction.patch().revision()),
                    )
                    .is_none_or(|entry_revision| entry_revision <= revision)
            }) {
                entry.persisted = true;
            }
            drop(pending);
            store.mark_workspace_cache_repaired(recovery_generation);
            return Ok(());
        }
    }

    async fn persist_pending_writes(
        &self,
        store: Option<&WorkspaceStore>,
    ) -> std::result::Result<(), StoreError> {
        if let Some(store) = store {
            if store.workspace_cache_needs_repair() {
                self.repair_workspace_cache_admitted(store).await?;
            }
        }
        loop {
            let next = self
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned")
                .iter()
                .enumerate()
                .find(|(_, entry)| !entry.persisted)
                .map(|(position, entry)| {
                    let batch = entry
                        .batch
                        .as_ref()
                        .expect("unpersisted workspace write must contain a store batch")
                        .clone();
                    (position, batch)
                });
            let Some((position, batch)) = next else {
                return Ok(());
            };
            let Some(store) = store else {
                return Err(StoreError::HubClosed);
            };
            let expected_claims = batch.notification_claims();

            // The queue entry deliberately remains installed across this await.
            // Cancellation can therefore only cause a harmless stale replay.
            let notification_claims = store
                .execute_store_batch_with_claims(batch)
                .await?
                .notification_claims;
            let complete_claim_results = expected_claims.iter().all(|expected| {
                notification_claims
                    .iter()
                    .any(|outcome| outcome.identity == *expected)
            });
            if !complete_claim_results {
                return Err(StoreError::rejected_update(
                    "notification delivery claim was not persisted or replayed",
                ));
            }
            let mut pending = self
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned");
            let entry = pending
                .get_mut(position)
                .expect("persisted reduction disappeared while admission was held");
            if notification_claims
                .iter()
                .any(|outcome| outcome.notification_claimed)
            {
                entry.notification_claimed = true;
            }
            entry.persisted = true;
        }
    }

    fn record_attention_persistence(
        &self,
        outcome: AttentionPersistenceOutcome,
        notification_claimed: bool,
    ) {
        self.attention_metrics
            .record_persistence(outcome, notification_claimed);
    }

    fn trace_attention_metrics_snapshot(&self) {
        self.attention_metrics.trace_snapshot();
    }

    fn attention_metrics_handle(&self) -> Arc<AttentionMetrics> {
        Arc::clone(&self.attention_metrics)
    }

    #[cfg(test)]
    fn attention_metrics_snapshot(&self) -> crate::attention_metrics::AttentionMetricsSnapshot {
        self.attention_metrics.snapshot()
    }

    fn update_attention_context(&self, context: WorkspaceAttentionContext) {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::AttentionContextChanged(context),
            );
    }

    fn update_attention_preferences(&self, preferences: AttentionPreferences) {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::AttentionPreferencesChanged(preferences),
            );
    }

    fn preview_message_attention(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
    ) -> Option<MessageAttentionEffect> {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .preview_message_attention(channel_id, message, kind, origin)
    }
}

#[derive(Clone, Debug)]
struct HuddleActorHandle {
    sender: mpsc::UnboundedSender<HuddleActorMessage>,
}

#[derive(Debug)]
enum HuddleActorMessage {
    Command(HuddleCommand),
    Input(CoordinatorInput),
}

impl HuddleActorHandle {
    fn command(&self, command: HuddleCommand) -> Result<()> {
        self.sender
            .send(HuddleActorMessage::Command(command))
            .map_err(|_| anyhow!("huddle coordinator is not available"))
    }

    fn input(&self, input: CoordinatorInput) -> Result<()> {
        self.sender
            .send(HuddleActorMessage::Input(input))
            .map_err(|_| anyhow!("huddle coordinator is not available"))
    }

    fn observe_huddle(&self, huddle: ActiveHuddle) -> Result<()> {
        self.input(CoordinatorInput::HuddleDiscovered(huddle))
    }
}

fn huddle_actor_channel() -> (
    HuddleActorHandle,
    mpsc::UnboundedReceiver<HuddleActorMessage>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (HuddleActorHandle { sender }, receiver)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrackedRequest {
    identity: RuntimeIdentity,
    context: OperationContext,
    supersedes_previous: bool,
    navigation_slot: Option<NavigationSlot>,
}

impl TrackedRequest {
    fn new(identity: RuntimeIdentity, context: OperationContext) -> Self {
        Self {
            identity,
            context,
            supersedes_previous: true,
            navigation_slot: None,
        }
    }

    fn for_command(identity: RuntimeIdentity, command: &RuntimeCommand) -> Self {
        Self {
            identity,
            context: command.operation_context(),
            supersedes_previous: command.supersedes_previous(),
            navigation_slot: command.navigation_slot(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveWork {
    Task {
        task_id: u64,
    },
    Sync {
        admission: AdmissionToken,
        cancellation_id: CancellationId,
    },
}

struct RuntimeTaskHandle {
    abort: tokio::task::AbortHandle,
    completion: watch::Receiver<bool>,
}

impl RuntimeTaskHandle {
    fn request_abort(&self) {
        self.abort.abort();
    }

    fn abort(self) -> watch::Receiver<bool> {
        self.request_abort();
        self.completion
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeSyncPlan {
    target: SyncTargetKey,
    priority: SyncPriority,
    durability: SyncDurability,
    freshness: FreshnessPolicy,
    replacement: ReplacementClass,
    retry: RetryPolicy,
}

impl RuntimeSyncPlan {
    const fn ephemeral(
        target: SyncTargetKey,
        priority: SyncPriority,
        replacement: ReplacementClass,
    ) -> Self {
        Self {
            target,
            priority,
            durability: SyncDurability::Ephemeral,
            freshness: FreshnessPolicy::Always,
            replacement,
            retry: RetryPolicy::Never,
        }
    }

    fn job(self, identity: crate::runtime_sync::RuntimeSyncJobIdentity) -> SyncJob {
        SyncJob::new(
            identity.job_id(),
            identity.cancellation_id(),
            self.target,
            self.priority,
            self.durability,
            self.freshness,
            self.replacement,
            self.retry,
        )
        .expect("runtime sync plan must satisfy scheduler job contracts")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeStartupSyncKind {
    EmojiCatalog,
    Membership,
    UserGroups,
}

struct PendingMembershipSync {
    session: SessionId,
    plan: RuntimeSyncPlan,
    work: RuntimeSyncWork,
}

fn runtime_sync_target(kind: SyncTargetKind, domain: &str, stable_parts: &[&str]) -> SyncTargetKey {
    fn update_part(hasher: &mut Sha256, value: &str) {
        let bytes = value.as_bytes();
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    }

    let mut hasher = Sha256::new();
    update_part(&mut hasher, "conduit-runtime-sync-target-v1");
    update_part(&mut hasher, domain);
    for part in stable_parts {
        update_part(&mut hasher, part);
    }
    let digest = hasher.finalize();
    let opaque_id = u64::from_be_bytes(
        digest[..std::mem::size_of::<u64>()]
            .try_into()
            .expect("SHA-256 digest contains a u64 prefix"),
    );
    SyncTargetKey::new(kind, opaque_id)
}

fn connected_command_sync_plan(command: &RuntimeCommand) -> Option<RuntimeSyncPlan> {
    let plan = match command {
        RuntimeCommand::RefreshConversations => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Workspace,
                "workspace-operation",
                &["conversation-membership"],
            ),
            SyncPriority::Foreground,
            ReplacementClass::Refresh(RefreshClass::Membership),
        ),
        RuntimeCommand::DiscoverConversations => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Workspace,
                "workspace-operation",
                &["conversation-discovery"],
            ),
            SyncPriority::Maintenance,
            ReplacementClass::Refresh(RefreshClass::Workspace),
        ),
        RuntimeCommand::DiscoverChannels => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Workspace,
                "workspace-operation",
                &["conversation-discovery"],
            ),
            SyncPriority::Interactive,
            ReplacementClass::Refresh(RefreshClass::Workspace),
        ),
        RuntimeCommand::LoadHistory { channel_id } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Conversation,
                "conversation-history",
                &[channel_id],
            ),
            SyncPriority::Interactive,
            ReplacementClass::Refresh(RefreshClass::ConversationHistory),
        ),
        RuntimeCommand::LoadOlderHistory { channel_id, .. } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Conversation,
                "conversation-history",
                &[channel_id],
            ),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        RuntimeCommand::LoadThread { channel_id, ts } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(SyncTargetKind::Thread, "thread-replies", &[channel_id, ts]),
            SyncPriority::Interactive,
            ReplacementClass::Refresh(RefreshClass::ThreadReplies),
        ),
        RuntimeCommand::LoadOlderThread { channel_id, ts, .. } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(SyncTargetKind::Thread, "thread-replies", &[channel_id, ts]),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        RuntimeCommand::LoadMessageContext(location) => {
            let (kind, parts, replacement) = match location.thread_ts() {
                Some(thread_ts) => (
                    SyncTargetKind::Thread,
                    vec![location.channel_id(), thread_ts, location.message_ts()],
                    ReplacementClass::Refresh(RefreshClass::ThreadReplies),
                ),
                None => (
                    SyncTargetKind::Conversation,
                    vec![location.channel_id(), location.message_ts()],
                    ReplacementClass::Refresh(RefreshClass::ConversationHistory),
                ),
            };
            RuntimeSyncPlan::ephemeral(
                runtime_sync_target(kind, "message-context", &parts),
                SyncPriority::Interactive,
                replacement,
            )
        }
        RuntimeCommand::SearchMessages { .. } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::SearchIndex,
                "workspace-operation",
                &["message-search"],
            ),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        RuntimeCommand::LoadFiles => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(SyncTargetKind::Workspace, "workspace-operation", &["files"]),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        RuntimeCommand::LoadFile { file_id, .. } => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(SyncTargetKind::Asset, "file", &[file_id]),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        RuntimeCommand::LoadSavedItems => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Workspace,
                "workspace-operation",
                &["saved-items"],
            ),
            SyncPriority::Interactive,
            ReplacementClass::Never,
        ),
        _ => return None,
    };
    Some(plan)
}

fn startup_sync_plan(kind: RuntimeStartupSyncKind) -> RuntimeSyncPlan {
    match kind {
        RuntimeStartupSyncKind::EmojiCatalog => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Asset,
                "workspace-operation",
                &["emoji-catalog"],
            ),
            SyncPriority::Maintenance,
            ReplacementClass::Refresh(RefreshClass::Workspace),
        ),
        RuntimeStartupSyncKind::Membership => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::Workspace,
                "workspace-operation",
                &["conversation-membership"],
            ),
            SyncPriority::Foreground,
            ReplacementClass::Refresh(RefreshClass::Membership),
        ),
        RuntimeStartupSyncKind::UserGroups => RuntimeSyncPlan::ephemeral(
            runtime_sync_target(
                SyncTargetKind::UserDirectory,
                "workspace-operation",
                &["user-groups"],
            ),
            SyncPriority::Maintenance,
            ReplacementClass::Refresh(RefreshClass::UserDirectory),
        ),
    }
}

fn runtime_sync_scheduler() -> RuntimeSyncScheduler {
    RuntimeSyncScheduler::new(
        SchedulerConfig::new(
            SYNC_TASK_ADMISSION_CAPACITY,
            SYNC_TASK_RUNNING_CAPACITY,
            SYNC_TASK_STARVATION_BOUND,
        )
        .expect("runtime sync scheduler configuration must be valid"),
    )
}

fn combine_runtime_shutdown_completions(
    mut completions: Vec<watch::Receiver<bool>>,
) -> Option<watch::Receiver<bool>> {
    completions.retain(|completion| !*completion.borrow());
    match completions.len() {
        0 => None,
        1 => completions.pop(),
        _ => {
            let (completed, receiver) = watch::channel(false);
            tokio::spawn(async move {
                for mut completion in completions {
                    while !*completion.borrow() {
                        if completion.changed().await.is_err() {
                            break;
                        }
                    }
                }
                completed.send_replace(true);
            });
            Some(receiver)
        }
    }
}

struct RuntimeState {
    active_session: SessionId,
    connection: Option<RuntimeConnection>,
    attention_preferences: AttentionPreferences,
    sync_scheduler: RuntimeSyncScheduler,
    socket_mode_supervisor: Option<SocketModeSupervisorHandle>,
    tasks: HashMap<u64, RuntimeTaskHandle>,
    task_requests: HashMap<u64, TrackedRequest>,
    active_requests: HashMap<OperationContext, ActiveWork>,
    latest_requests: HashMap<OperationContext, RequestId>,
    active_navigation: HashMap<NavigationSlot, ActiveWork>,
    latest_navigation: HashMap<NavigationSlot, RequestId>,
    pending_membership: Option<PendingMembershipSync>,
    next_task_id: u64,
}

impl RuntimeState {
    fn new(active_session: SessionId) -> Self {
        Self {
            active_session,
            connection: None,
            attention_preferences: AttentionPreferences::default(),
            sync_scheduler: runtime_sync_scheduler(),
            socket_mode_supervisor: None,
            tasks: HashMap::new(),
            task_requests: HashMap::new(),
            active_requests: HashMap::new(),
            latest_requests: HashMap::new(),
            active_navigation: HashMap::new(),
            latest_navigation: HashMap::new(),
            pending_membership: None,
            next_task_id: 0,
        }
    }

    fn begin_session_replacement(&mut self) -> Option<watch::Receiver<bool>> {
        let sync_completion = self.sync_scheduler.begin_shutdown();
        let task_completions = self
            .tasks
            .drain()
            .map(|(_, task)| task.abort())
            .collect::<Vec<_>>();
        self.active_requests.clear();
        self.latest_requests.clear();
        self.task_requests.clear();
        self.active_navigation.clear();
        self.latest_navigation.clear();
        self.pending_membership = None;
        self.connection = None;
        let socket_completion = self
            .socket_mode_supervisor
            .as_ref()
            .map(SocketModeSupervisorHandle::cancel);
        combine_runtime_shutdown_completions(
            socket_completion
                .into_iter()
                .chain(task_completions)
                .chain([sync_completion])
                .collect(),
        )
    }

    fn finish_session_replacement(&mut self, session: SessionId) {
        if let Some(supervisor) = self.socket_mode_supervisor.take() {
            supervisor.task.abort();
        }
        self.sync_scheduler = runtime_sync_scheduler();
        self.active_session = session;
    }

    fn register_socket_mode_supervisor(
        &mut self,
        session: SessionId,
        task_id: u64,
        task: tokio::task::AbortHandle,
        cancellation: watch::Sender<bool>,
        completion: watch::Receiver<bool>,
    ) -> bool {
        if self.active_session != session
            || task.is_finished()
            || self.socket_mode_supervisor.is_some()
        {
            task.abort();
            return false;
        }
        self.socket_mode_supervisor = Some(SocketModeSupervisorHandle {
            task_id,
            task,
            cancellation,
            completion,
        });
        true
    }

    fn finish_socket_mode_supervisor(&mut self, task_id: u64) {
        if self
            .socket_mode_supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.task_id == task_id)
        {
            self.socket_mode_supervisor = None;
        }
    }

    fn set_attention_preferences(&mut self, preferences: AttentionPreferences) {
        self.attention_preferences = preferences.clone();
        if let Some(connection) = self.connection.as_ref() {
            connection
                .workspace
                .update_attention_preferences(preferences);
        }
    }

    fn attention_context(&self, current_user_id: Option<String>) -> WorkspaceAttentionContext {
        WorkspaceAttentionContext { current_user_id }
    }

    fn next_task_id(&mut self) -> u64 {
        self.next_task_id = self.next_task_id.saturating_add(1);
        self.next_task_id
    }

    fn register_task_with_completion(
        &mut self,
        session: SessionId,
        task_id: u64,
        request: Option<TrackedRequest>,
        task: tokio::task::AbortHandle,
        completion: watch::Receiver<bool>,
    ) -> bool {
        if self.active_session != session || task.is_finished() {
            task.abort();
            return false;
        }

        if let Some(request) = request.as_ref() {
            if request.identity.session != session {
                task.abort();
                return false;
            }
            if request.supersedes_previous
                && self
                    .latest_requests
                    .get(&request.context)
                    .is_some_and(|latest| *latest >= request.identity.request)
            {
                task.abort();
                return false;
            }
            if let Some(slot) = request.navigation_slot {
                if self
                    .latest_navigation
                    .get(&slot)
                    .is_some_and(|latest| *latest >= request.identity.request)
                {
                    task.abort();
                    return false;
                }
            }

            let context_task = request
                .supersedes_previous
                .then(|| self.active_requests.get(&request.context).copied())
                .flatten();
            let navigation_work = request
                .navigation_slot
                .and_then(|slot| self.active_navigation.get(&slot).copied());
            if let Some(previous) = context_task {
                self.cancel_active_work(previous);
            }
            if let Some(previous) = navigation_work {
                if Some(previous) != context_task {
                    self.cancel_active_work(previous);
                }
            }

            if request.supersedes_previous {
                self.latest_requests
                    .insert(request.context.clone(), request.identity.request);
                self.active_requests
                    .insert(request.context.clone(), ActiveWork::Task { task_id });
            }
            if let Some(slot) = request.navigation_slot {
                self.latest_navigation
                    .insert(slot, request.identity.request);
                self.active_navigation
                    .insert(slot, ActiveWork::Task { task_id });
            }
            self.task_requests.insert(task_id, request.clone());
        }

        self.tasks.insert(
            task_id,
            RuntimeTaskHandle {
                abort: task,
                completion,
            },
        );
        true
    }

    #[cfg(test)]
    fn register_task(
        &mut self,
        session: SessionId,
        task_id: u64,
        request: Option<TrackedRequest>,
        task: tokio::task::AbortHandle,
    ) -> bool {
        let (_, completion) = watch::channel(true);
        self.register_task_with_completion(session, task_id, request, task, completion)
    }

    fn finish_task(&mut self, task_id: u64, request: Option<&TrackedRequest>) {
        self.tasks.remove(&task_id);
        self.task_requests.remove(&task_id);
        if let Some(request) = request {
            if request.supersedes_previous {
                let is_current = self
                    .active_requests
                    .get(&request.context)
                    .is_some_and(|active| *active == ActiveWork::Task { task_id });
                if is_current {
                    self.active_requests.remove(&request.context);
                }
            }
            if let Some(slot) = request.navigation_slot {
                let is_current = self
                    .active_navigation
                    .get(&slot)
                    .is_some_and(|active| *active == ActiveWork::Task { task_id });
                if is_current {
                    self.active_navigation.remove(&slot);
                }
            }
        }
    }

    fn abort_task(&mut self, task_id: u64) {
        if let Some(task) = self.tasks.get(&task_id) {
            task.request_abort();
        }
        if let Some(request) = self.task_requests.remove(&task_id) {
            if request.supersedes_previous
                && self
                    .active_requests
                    .get(&request.context)
                    .is_some_and(|active| *active == ActiveWork::Task { task_id })
            {
                self.active_requests.remove(&request.context);
            }
            if let Some(slot) = request.navigation_slot {
                if self
                    .active_navigation
                    .get(&slot)
                    .is_some_and(|active| *active == ActiveWork::Task { task_id })
                {
                    self.active_navigation.remove(&slot);
                }
            }
        }
    }

    fn cancel_active_work(&mut self, active: ActiveWork) {
        match active {
            ActiveWork::Task { task_id } => self.abort_task(task_id),
            ActiveWork::Sync {
                admission,
                cancellation_id,
            } => {
                let _ = self.sync_scheduler.cancel(cancellation_id);
                self.active_requests.retain(|_, current| {
                    *current
                        != ActiveWork::Sync {
                            admission,
                            cancellation_id,
                        }
                });
                self.active_navigation.retain(|_, current| {
                    *current
                        != ActiveWork::Sync {
                            admission,
                            cancellation_id,
                        }
                });
            }
        }
    }

    fn admit_sync_request(
        &mut self,
        request: &TrackedRequest,
        plan: RuntimeSyncPlan,
        work: RuntimeSyncWork,
    ) -> RuntimeSyncRequestAdmission {
        if self.active_session != request.identity.session {
            return RuntimeSyncRequestAdmission::Stale;
        }
        if request.supersedes_previous
            && self
                .latest_requests
                .get(&request.context)
                .is_some_and(|latest| *latest >= request.identity.request)
        {
            return RuntimeSyncRequestAdmission::Stale;
        }
        if let Some(slot) = request.navigation_slot {
            if self
                .latest_navigation
                .get(&slot)
                .is_some_and(|latest| *latest >= request.identity.request)
            {
                return RuntimeSyncRequestAdmission::Stale;
            }
        }

        let cancel_context_work = request.supersedes_previous
            && (request.navigation_slot.is_some() || plan.replacement == ReplacementClass::Never);
        let context_work = cancel_context_work
            .then(|| self.active_requests.get(&request.context).copied())
            .flatten();
        let navigation_work = request
            .navigation_slot
            .and_then(|slot| self.active_navigation.get(&slot).copied());
        if let Some(previous) = context_work {
            self.cancel_active_work(previous);
        }
        if let Some(previous) = navigation_work {
            if Some(previous) != context_work {
                self.cancel_active_work(previous);
            }
        }

        if request.supersedes_previous {
            self.latest_requests
                .insert(request.context.clone(), request.identity.request);
        }
        if let Some(slot) = request.navigation_slot {
            self.latest_navigation
                .insert(slot, request.identity.request);
        }

        let identity = self.sync_scheduler.allocate_job_identity();
        let cancellation_id = identity.cancellation_id();
        match self.sync_scheduler.admit(plan.job(identity), None, work) {
            Ok(RuntimeSyncAdmissionOutcome::Accepted(receipt)) => {
                if plan.replacement == ReplacementClass::Refresh(RefreshClass::Membership) {
                    self.pending_membership = None;
                }
                let active = ActiveWork::Sync {
                    admission: receipt.admission(),
                    cancellation_id,
                };
                if request.supersedes_previous {
                    self.active_requests.insert(request.context.clone(), active);
                }
                if let Some(slot) = request.navigation_slot {
                    self.active_navigation.insert(slot, active);
                }
                RuntimeSyncRequestAdmission::Accepted(receipt)
            }
            Ok(RuntimeSyncAdmissionOutcome::SkippedFresh(_)) => {
                RuntimeSyncRequestAdmission::SkippedFresh
            }
            Err(error) => RuntimeSyncRequestAdmission::Rejected(error.reason()),
        }
    }

    fn admit_session_sync(
        &mut self,
        session: SessionId,
        plan: RuntimeSyncPlan,
        work: RuntimeSyncWork,
    ) -> RuntimeSyncRequestAdmission {
        if self.active_session != session {
            return RuntimeSyncRequestAdmission::Stale;
        }
        let identity = self.sync_scheduler.allocate_job_identity();
        match self.sync_scheduler.admit(plan.job(identity), None, work) {
            Ok(RuntimeSyncAdmissionOutcome::Accepted(receipt)) => {
                if plan.replacement == ReplacementClass::Refresh(RefreshClass::Membership) {
                    self.pending_membership = None;
                }
                RuntimeSyncRequestAdmission::Accepted(receipt)
            }
            Ok(RuntimeSyncAdmissionOutcome::SkippedFresh(_)) => {
                RuntimeSyncRequestAdmission::SkippedFresh
            }
            Err(error) => RuntimeSyncRequestAdmission::Rejected(error.reason()),
        }
    }

    fn finish_sync_request(
        &mut self,
        session: SessionId,
        admission: AdmissionToken,
        request: Option<&TrackedRequest>,
    ) {
        if self.active_session != session {
            return;
        }
        let Some(request) = request else {
            return;
        };
        if request.supersedes_previous
            && self
                .active_requests
                .get(&request.context)
                .is_some_and(|active| {
                    matches!(
                        active,
                        ActiveWork::Sync {
                            admission: current,
                            ..
                        } if *current == admission
                    )
                })
        {
            self.active_requests.remove(&request.context);
        }
        if let Some(slot) = request.navigation_slot {
            if self.active_navigation.get(&slot).is_some_and(|active| {
                matches!(
                    active,
                    ActiveWork::Sync {
                        admission: current,
                        ..
                    } if *current == admission
                )
            }) {
                self.active_navigation.remove(&slot);
            }
        }
    }
}

enum RuntimeSyncRequestAdmission {
    Accepted(RuntimeSyncReceipt),
    SkippedFresh,
    Stale,
    Rejected(AdmissionRejectionReason),
}

struct SocketModeSupervisorHandle {
    task_id: u64,
    task: tokio::task::AbortHandle,
    cancellation: watch::Sender<bool>,
    completion: watch::Receiver<bool>,
}

impl SocketModeSupervisorHandle {
    fn cancel(&self) -> watch::Receiver<bool> {
        let _ = self.cancellation.send(true);
        self.completion.clone()
    }
}

struct SocketModeSupervisorCompletion(Option<watch::Sender<bool>>);

impl Drop for SocketModeSupervisorCompletion {
    fn drop(&mut self) {
        if let Some(completion) = self.0.take() {
            let _ = completion.send(true);
        }
    }
}

struct RuntimeTaskCompletion {
    completion: Option<watch::Sender<bool>>,
    state: Arc<Mutex<RuntimeState>>,
    task_id: u64,
    request: Option<TrackedRequest>,
}

impl Drop for RuntimeTaskCompletion {
    fn drop(&mut self) {
        self.state
            .lock()
            .expect("runtime state lock poisoned")
            .finish_task(self.task_id, self.request.as_ref());
        if let Some(completion) = self.completion.take() {
            completion.send_replace(true);
        }
    }
}

async fn replace_runtime_session(state: &Arc<Mutex<RuntimeState>>, session: SessionId) {
    let (completion, workspace_store) = {
        let mut runtime_state = state.lock().expect("runtime state lock poisoned");
        let workspace_store = runtime_state
            .connection
            .as_ref()
            .and_then(|connection| connection.workspace_store.clone());
        (runtime_state.begin_session_replacement(), workspace_store)
    };
    if let Some(mut completion) = completion {
        while !*completion.borrow() {
            if completion.changed().await.is_err() {
                break;
            }
        }
    }
    if let Some(store) = workspace_store {
        if let Err(error) = store.barrier().await {
            crate::debug::log(
                "store",
                &format!(
                    "WorkspaceSessionReplacementBarrierFailed category={:?}",
                    error.category()
                ),
            );
        }
    }
    state
        .lock()
        .expect("runtime state lock poisoned")
        .finish_session_replacement(session);
}

fn spawn_session_task<F>(state: &Arc<Mutex<RuntimeState>>, session: SessionId, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_runtime_task(state, session, None, future);
}

fn spawn_request_task<F>(state: &Arc<Mutex<RuntimeState>>, request: TrackedRequest, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_runtime_task(state, request.identity.session, Some(request), future);
}

fn spawn_runtime_task<F>(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    request: Option<TrackedRequest>,
    future: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let task_id = state
        .lock()
        .expect("runtime state lock poisoned")
        .next_task_id();
    let state_after_task = Arc::clone(state);
    let request_after_task = request.clone();
    let (start_task, task_started) = tokio::sync::oneshot::channel();
    let (completion, completion_receiver) = watch::channel(false);
    let parent_span = tracing::Span::current();
    let task = tokio::spawn(
        async move {
            let _completion = RuntimeTaskCompletion {
                completion: Some(completion),
                state: state_after_task,
                task_id,
                request: request_after_task,
            };
            // Locals drop in reverse declaration order. Keep the user future
            // after the completion guard so its store handles are retired
            // before session replacement observes task completion.
            let future = future;
            if task_started.await.is_err() {
                return;
            }
            future.await;
        }
        .instrument(parent_span),
    );
    let registered = state
        .lock()
        .expect("runtime state lock poisoned")
        .register_task_with_completion(
            session,
            task_id,
            request,
            task.abort_handle(),
            completion_receiver,
        );
    if registered {
        let _ = start_task.send(());
    }
}

fn spawn_socket_mode_supervisor<F, Fut>(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    future: F,
) where
    F: FnOnce(watch::Receiver<bool>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let task_id = state
        .lock()
        .expect("runtime state lock poisoned")
        .next_task_id();
    let state_after_task = Arc::clone(state);
    let (start_task, task_started) = oneshot::channel();
    let (cancellation, cancellation_receiver) = watch::channel(false);
    let (completion, completion_receiver) = watch::channel(false);
    let parent_span = tracing::Span::current();
    let task = tokio::spawn(
        async move {
            let _completion = SocketModeSupervisorCompletion(Some(completion));
            if task_started.await.is_err() {
                return;
            }
            future(cancellation_receiver).await;
            state_after_task
                .lock()
                .expect("runtime state lock poisoned")
                .finish_socket_mode_supervisor(task_id);
        }
        .instrument(parent_span),
    );
    let registered = state
        .lock()
        .expect("runtime state lock poisoned")
        .register_socket_mode_supervisor(
            session,
            task_id,
            task.abort_handle(),
            cancellation,
            completion_receiver,
        );
    if registered {
        let _ = start_task.send(());
    }
}

#[derive(Clone, Debug)]
pub struct AppRuntime {
    commands: mpsc::UnboundedSender<RuntimeRequest>,
}

#[derive(Clone, Debug)]
struct ImageAssetCache {
    directory: PathBuf,
}

impl ImageAssetCache {
    fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    async fn load(&self, key: &str) -> Result<Option<String>> {
        let path = self.path_for_key(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(data_uri)
                if data_uri.starts_with("data:image/") || data_uri.starts_with("data:video/") =>
            {
                Ok(Some(data_uri))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read cached image {}", path.display())),
        }
    }

    async fn store(&self, key: &str, data_uri: &str) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| {
                format!(
                    "failed to create image cache directory {}",
                    self.directory.display()
                )
            })?;

        let path = self.path_for_key(key);
        tokio::fs::write(&path, data_uri)
            .await
            .with_context(|| format!("failed to write cached image {}", path.display()))
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.directory
            .join(format!("{}.data-uri", image_asset_cache_key(key)))
    }
}

fn image_asset_cache_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn media_cache_path(url: &str, name: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        });
    let filename = extension.map_or(key.clone(), |extension| format!("{key}.{extension}"));
    config::media_cache_dir().join(filename)
}

fn attachment_cache_path(url: &str, name: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let basename = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let basename = truncate_utf8(&basename, ATTACHMENT_BASENAME_MAX_BYTES);
    let basename = basename.trim_matches([' ', '.']).trim();
    let basename = if basename.is_empty() {
        "attachment"
    } else {
        basename
    };
    config::attachment_cache_dir().join(format!("{key}-{basename}"))
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[derive(Clone, Copy)]
struct AttachmentCachePolicy {
    max_age: Duration,
    max_bytes: u64,
}

impl Default for AttachmentCachePolicy {
    fn default() -> Self {
        Self {
            max_age: ATTACHMENT_CACHE_MAX_AGE,
            max_bytes: ATTACHMENT_CACHE_MAX_BYTES,
        }
    }
}

struct AttachmentCacheEntry {
    path: PathBuf,
    size: u64,
    last_used: SystemTime,
}

fn prune_attachment_cache(
    directory: &Path,
    protected: Option<&Path>,
    policy: AttachmentCachePolicy,
    now: SystemTime,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut retained = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let is_protected = protected.is_some_and(|protected| protected == path);
        let last_used = metadata
            .accessed()
            .ok()
            .into_iter()
            .chain(metadata.modified().ok())
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let expired = now
            .duration_since(last_used)
            .is_ok_and(|age| age > policy.max_age);
        if expired && !is_protected {
            let _ = std::fs::remove_file(path);
            continue;
        }
        // A concurrent download writes to a process-specific `.part` file.
        // Never include an active partial in size eviction; age cleanup still
        // removes abandoned partials left behind by an interrupted process.
        if path
            .extension()
            .is_some_and(|extension| extension == "part")
        {
            continue;
        }
        retained.push(AttachmentCacheEntry {
            path,
            size: metadata.len(),
            last_used,
        });
    }

    let mut total = retained
        .iter()
        .fold(0_u64, |total, entry| total.saturating_add(entry.size));
    retained.sort_by(|left, right| {
        left.last_used
            .cmp(&right.last_used)
            .then_with(|| left.path.cmp(&right.path))
    });
    for entry in retained {
        if total <= policy.max_bytes {
            break;
        }
        if protected.is_some_and(|protected| protected == entry.path) {
            continue;
        }
        if std::fs::remove_file(&entry.path).is_ok() {
            total = total.saturating_sub(entry.size);
        }
    }

    Ok(())
}

async fn maintain_attachment_cache(protected: Option<PathBuf>) {
    let directory = config::attachment_cache_dir();
    let result = tokio::task::spawn_blocking(move || {
        prune_attachment_cache(
            &directory,
            protected.as_deref(),
            AttachmentCachePolicy::default(),
            SystemTime::now(),
        )
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => crate::debug::log(
            "runtime",
            &format!("AttachmentCacheCleanupFailed error={error}"),
        ),
        Err(error) => crate::debug::log(
            "runtime",
            &format!("AttachmentCacheCleanupTaskFailed error={error}"),
        ),
    }
}

struct RemoveFileOnDrop(Option<PathBuf>);

impl RemoveFileOnDrop {
    fn new(enabled: bool, path: &Path) -> Self {
        Self(enabled.then(|| path.to_path_buf()))
    }
}

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn preview_asset_data_uri(asset: DownloadedPreviewAsset) -> String {
    format!(
        "data:{};base64,{}",
        asset.mime_type,
        BASE64.encode(asset.bytes)
    )
}

impl AppRuntime {
    pub fn start() -> (Self, mpsc::UnboundedReceiver<RuntimeEvent>) {
        let (commands, receiver) = mpsc::unbounded_channel::<RuntimeRequest>();
        let (events, event_receiver) = mpsc::unbounded_channel::<RuntimeEvent>();

        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    crate::debug::log("runtime", &format!("RuntimeStartFailed error={error:#}"));
                    let error = anyhow::Error::new(error);
                    let kind =
                        RuntimeEventKind::RuntimeStartFailed(RuntimeFailure::from_error(&error));
                    let context =
                        OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace);
                    let meta = RuntimeEventMeta::new(
                        RuntimeIdentity {
                            session: SessionId::default().next(),
                            request: RequestId::new(1),
                        },
                        context,
                    );
                    let _ = events.send(RuntimeEvent {
                        meta: meta.clone(),
                        kind: RuntimeEventKind::WorkspaceLifecycle(
                            WorkspaceLifecycleEvent::StartupFailed,
                        ),
                    });
                    let _ = events.send(RuntimeEvent { meta, kind });
                    return;
                }
            };

            runtime.block_on(run_runtime(receiver, events));
        });

        (Self { commands }, event_receiver)
    }

    pub fn send(&self, identity: RuntimeIdentity, command: RuntimeCommand) {
        let _ = self.commands.send(RuntimeRequest { identity, command });
    }
}

async fn run_runtime(
    mut commands: mpsc::UnboundedReceiver<RuntimeRequest>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
) {
    maintain_attachment_cache(None).await;
    let state = Arc::new(Mutex::new(RuntimeState::new(SessionId::default())));
    let oauth = SlackOAuthClient::new();
    let image_cache = ImageAssetCache::new(config::image_asset_cache_dir());
    let limits = RuntimeTaskLimits::new(
        NAVIGATION_TASK_CONCURRENCY,
        INTERACTIVE_TASK_CONCURRENCY,
        BACKGROUND_TASK_CONCURRENCY,
        IMAGE_TASK_CONCURRENCY,
        UPLOAD_TASK_CONCURRENCY,
    );

    while let Some(request) = commands.recv().await {
        let RuntimeRequest { identity, command } = request;
        let active_session = state
            .lock()
            .expect("runtime state lock poisoned")
            .active_session;
        if identity.session < active_session {
            continue;
        }
        if identity.session > active_session {
            replace_runtime_session(&state, identity.session).await;
        }
        let trace_fields = RuntimeTraceFields::for_command(identity, &command);
        let span = trace_fields.span();
        let _entered = span.enter();

        let event_sender =
            RuntimeEventSender::new(events.clone(), identity, command.operation_context());
        dispatch_command(
            command,
            identity,
            event_sender,
            &state,
            &oauth,
            &image_cache,
            &limits,
        );
    }

    replace_runtime_session(&state, SessionId::default()).await;
}

fn dispatch_command(
    command: RuntimeCommand,
    identity: RuntimeIdentity,
    events: RuntimeEventSender,
    state: &Arc<Mutex<RuntimeState>>,
    oauth: &SlackOAuthClient,
    image_cache: &ImageAssetCache,
    limits: &RuntimeTaskLimits,
) {
    match command {
        RuntimeCommand::LoadStoredToken => {
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::ConnectRequested,
            ));
            events.send_status("Checking secure storage");
            let token = match TokenStore.load() {
                Ok(Some(token)) => {
                    if token.should_refresh() {
                        events.send_status("Refreshing Slack session");
                    }
                    Some(token)
                }
                Ok(None) => match browser_session_token_from_env() {
                    Ok(Some(token)) => {
                        events.send_status("Importing Slack browser session");
                        Some(token)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        send_lifecycle_failure(&events, &error);
                        return;
                    }
                },
                Err(error) => {
                    send_lifecycle_failure(&events, &error);
                    return;
                }
            };

            let Some(token) = token else {
                events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                    WorkspaceLifecycleEvent::SignedOut,
                ));
                events.send_event(RuntimeEventKind::SignedOut);
                return;
            };
            let failure_context = if token.browser_cookie_d.is_some() {
                AuthenticationFailureContext::BrowserSession
            } else {
                AuthenticationFailureContext::Default
            };
            let oauth = oauth.clone();
            spawn_authentication_task(state, identity, events, failure_context, async move {
                let token = if token.should_refresh() {
                    oauth.refresh(&token).await?
                } else {
                    token
                };
                authenticate_token(token).await
            });
        }
        RuntimeCommand::StartOAuth {
            client_id,
            debug_auth,
        } => {
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::ConnectRequested,
            ));
            events.send_status("Opening Slack authorization");
            let oauth = oauth.clone();
            spawn_authentication_task(
                state,
                identity,
                events,
                AuthenticationFailureContext::Default,
                async move {
                    let token = oauth
                        .authenticate(OAuthConfig::new(client_id), debug_auth)
                        .await?;
                    authenticate_token(token).await
                },
            );
        }
        RuntimeCommand::StartBrowserSession {
            xoxc_token,
            xoxd_token,
            user_agent,
        } => {
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::ConnectRequested,
            ));
            events.send_status("Validating Slack browser session");
            let token = match browser_session_token_from_values(
                Some(xoxc_token),
                Some(xoxd_token),
                user_agent,
            ) {
                Ok(Some(token)) => token,
                Ok(None) => {
                    let failure = RuntimeFailure::validation("Enter XOXC and XOXD tokens");
                    events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                        lifecycle_failure_event(&failure),
                    ));
                    events.send_event(RuntimeEventKind::Error(failure));
                    return;
                }
                Err(error) => {
                    let failure = RuntimeFailure::validation(error.to_string());
                    send_lifecycle_failure_with(&events, &error, failure);
                    return;
                }
            };
            spawn_authentication_task(
                state,
                identity,
                events,
                AuthenticationFailureContext::BrowserSession,
                authenticate_token(token),
            );
        }
        RuntimeCommand::SignOut => {
            finish_sign_out(&events, TokenStore.clear());
        }
        RuntimeCommand::Disconnect => {
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::SignedOut,
            ));
        }
        RuntimeCommand::UpdateAttentionPreferences(preferences) => {
            state
                .lock()
                .expect("runtime state lock poisoned")
                .set_attention_preferences(preferences);
        }
        command => {
            let connection = state
                .lock()
                .expect("runtime state lock poisoned")
                .connection
                .clone();
            let Some(connection) = connection else {
                events.send_event(RuntimeEventKind::Error(RuntimeFailure::validation(
                    "No Slack workspace is available",
                )));
                return;
            };
            if let Some(plan) = connected_command_sync_plan(&command) {
                schedule_connected_sync_command(
                    state,
                    identity,
                    command,
                    connection,
                    events,
                    image_cache.clone(),
                    plan,
                );
                return;
            }
            let lane = command.task_lane();
            let tracked_request = TrackedRequest::for_command(identity, &command);
            let image_cache = image_cache.clone();
            let limits = limits.clone();
            spawn_request_task(state, tracked_request, async move {
                let _permit = limits.acquire(lane).await;
                if let Err(error) =
                    handle_connected_command(command, connection, &events, &image_cache).await
                {
                    events.send_failure(&error);
                }
            });
        }
    }
}

fn schedule_connected_sync_command(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    command: RuntimeCommand,
    connection: RuntimeConnection,
    events: RuntimeEventSender,
    image_cache: ImageAssetCache,
    plan: RuntimeSyncPlan,
) {
    let request = TrackedRequest::for_command(identity, &command);
    let command_reports_failure = matches!(command, RuntimeCommand::RefreshConversations);
    let work_events = events.clone();
    let work = RuntimeSyncWork::new(move |_attempt| {
        let command = command.clone();
        let connection = connection.clone();
        let events = work_events.clone();
        let image_cache = image_cache.clone();
        async move {
            match handle_connected_command(command, connection, &events, &image_cache).await {
                Ok(()) => JobOutcome::Succeeded,
                Err(error) => {
                    if !command_reports_failure {
                        events.send_failure(&error);
                    }
                    JobOutcome::PermanentFailure
                }
            }
        }
    });
    let admission = state
        .lock()
        .expect("runtime state lock poisoned")
        .admit_sync_request(&request, plan, work);
    match admission {
        RuntimeSyncRequestAdmission::Accepted(receipt) => {
            spawn_runtime_sync_receipt_monitor(
                state,
                identity.session,
                Some(request),
                Some(events),
                receipt,
            );
        }
        RuntimeSyncRequestAdmission::SkippedFresh | RuntimeSyncRequestAdmission::Stale => {}
        RuntimeSyncRequestAdmission::Rejected(AdmissionRejectionReason::ShuttingDown) => {}
        RuntimeSyncRequestAdmission::Rejected(reason) => {
            crate::debug::log(
                "runtime",
                &format!("RuntimeSyncAdmissionRejected reason={reason:?}"),
            );
            events.send_event(RuntimeEventKind::Error(RuntimeFailure::internal()));
        }
    }
}

fn schedule_session_sync_work(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    plan: RuntimeSyncPlan,
    work: RuntimeSyncWork,
) {
    let retained_work = work.clone();
    let admission = state
        .lock()
        .expect("runtime state lock poisoned")
        .admit_session_sync(session, plan, work);
    match admission {
        RuntimeSyncRequestAdmission::Accepted(receipt) => {
            spawn_runtime_sync_receipt_monitor(state, session, None, None, receipt);
        }
        RuntimeSyncRequestAdmission::SkippedFresh | RuntimeSyncRequestAdmission::Stale => {}
        RuntimeSyncRequestAdmission::Rejected(AdmissionRejectionReason::AtCapacity)
            if plan.replacement == ReplacementClass::Refresh(RefreshClass::Membership) =>
        {
            retain_pending_membership_sync(state, session, plan, retained_work);
        }
        RuntimeSyncRequestAdmission::Rejected(reason) => {
            crate::debug::log(
                "runtime",
                &format!("RuntimeBackgroundSyncAdmissionRejected reason={reason:?}"),
            );
        }
    }
}

fn spawn_runtime_sync_receipt_monitor(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    request: Option<TrackedRequest>,
    failure_events: Option<RuntimeEventSender>,
    receipt: RuntimeSyncReceipt,
) {
    let admission = receipt.admission();
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let terminal = receipt.wait().await;
        state
            .lock()
            .expect("runtime state lock poisoned")
            .finish_sync_request(session, admission, request.as_ref());
        let internal_failure = match terminal {
            Ok(terminal) => matches!(
                terminal.result(),
                RuntimeSyncTerminalResult::Failed(RuntimeSyncFailureKind::Panicked)
            ),
            Err(_) => true,
        };
        if internal_failure {
            crate::debug::log("runtime", "RuntimeSyncWorkFailed reason=internal");
            if let Some(events) = failure_events {
                events.send_event(RuntimeEventKind::Error(RuntimeFailure::internal()));
            }
        }
        try_admit_pending_membership_sync(&state);
    });
}

fn retain_pending_membership_sync(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    plan: RuntimeSyncPlan,
    work: RuntimeSyncWork,
) {
    let mut state = state.lock().expect("runtime state lock poisoned");
    if state.active_session != session {
        return;
    }
    let replace = state.pending_membership.as_ref().is_none_or(|pending| {
        pending.session != session
            || sync_priority_strength(plan.priority)
                >= sync_priority_strength(pending.plan.priority)
    });
    if replace {
        state.pending_membership = Some(PendingMembershipSync {
            session,
            plan,
            work,
        });
    }
}

fn try_admit_pending_membership_sync(state: &Arc<Mutex<RuntimeState>>) {
    let (session, plan, retained_work, admission) = {
        let mut runtime_state = state.lock().expect("runtime state lock poisoned");
        let Some(pending) = runtime_state.pending_membership.take() else {
            return;
        };
        let PendingMembershipSync {
            session,
            plan,
            work,
        } = pending;
        let retained_work = work.clone();
        let admission = runtime_state.admit_session_sync(session, plan, work);
        (session, plan, retained_work, admission)
    };
    match admission {
        RuntimeSyncRequestAdmission::Accepted(receipt) => {
            spawn_runtime_sync_receipt_monitor(state, session, None, None, receipt);
        }
        RuntimeSyncRequestAdmission::Rejected(AdmissionRejectionReason::AtCapacity) => {
            retain_pending_membership_sync(state, session, plan, retained_work);
        }
        RuntimeSyncRequestAdmission::SkippedFresh
        | RuntimeSyncRequestAdmission::Stale
        | RuntimeSyncRequestAdmission::Rejected(_) => {}
    }
}

const fn sync_priority_strength(priority: SyncPriority) -> u8 {
    match priority {
        SyncPriority::Maintenance => 1,
        SyncPriority::Foreground => 2,
        SyncPriority::Interactive => 3,
    }
}

fn finish_sign_out(events: &RuntimeEventSender, clear_result: Result<()>) {
    if let Err(error) = crate::store::clear_active_workspace(&config::state_cache_dir()) {
        crate::debug::log(
            "store",
            &format!("ActiveWorkspaceClearFailed error={error:#}"),
        );
    }
    if let Err(error) = clear_result {
        events.send_failure(&error);
    }
    events.send_event(RuntimeEventKind::WorkspaceLifecycle(
        WorkspaceLifecycleEvent::SignedOut,
    ));
    events.send_event(RuntimeEventKind::SignedOut);
}

async fn authenticate_token(token: StoredToken) -> Result<(StoredToken, SlackApi, AuthInfo)> {
    let token_team = token.team_name.clone().or(token.team_id.clone());
    let token_team_id = token.team_id.clone();
    let token_user = token.user_id.clone();
    let api = SlackApi::new(token.clone());
    let mut auth = api.auth_test().await?;
    auth.team = auth.team.or(token_team);
    auth.team_id = auth.team_id.or(token_team_id);
    auth.user_id = auth.user_id.or(token_user);
    crate::debug::log(
        "runtime",
        &format!(
            "Authenticated team={} user_id={}",
            auth.team.as_deref().unwrap_or("<unknown>"),
            auth.user_id.as_deref().unwrap_or("<unknown>")
        ),
    );
    Ok((token, api, auth))
}

fn spawn_authentication_task<F>(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    events: RuntimeEventSender,
    failure_context: AuthenticationFailureContext,
    future: F,
) where
    F: Future<Output = Result<(StoredToken, SlackApi, AuthInfo)>> + Send + 'static,
{
    let state_for_task = Arc::clone(state);
    spawn_request_task(
        state,
        TrackedRequest::new(
            identity,
            OperationContext::new(RuntimeOperation::Authenticate, RuntimeTarget::Workspace),
        ),
        async move {
            let result = future.await;
            match result {
                Ok((token, api, auth)) => {
                    let (huddles, huddle_receiver) = huddle_actor_channel();
                    let connection = {
                        let mut runtime_state =
                            state_for_task.lock().expect("runtime state lock poisoned");
                        if runtime_state.active_session != identity.session {
                            return;
                        }
                        if let Err(error) = TokenStore.save(&token) {
                            send_lifecycle_failure(&events, &error);
                            return;
                        }
                        let workspace = WorkspaceReducerAdapter::default();
                        workspace.update_attention_context(
                            runtime_state.attention_context(auth.user_id.clone()),
                        );
                        workspace.update_attention_preferences(
                            runtime_state.attention_preferences.clone(),
                        );
                        let connection = RuntimeConnection {
                            slack: api,
                            workspace_url: auth.url.clone(),
                            workspace_store: Some(WorkspaceStore::new(
                                config::state_cache_dir(),
                                &workspace_store_id(&auth),
                            )),
                            workspace,
                            current_user_id: auth.user_id.clone(),
                            user_cache: Arc::new(Mutex::new(HashMap::new())),
                            read_marks: Arc::new(Mutex::new(HashMap::new())),
                            message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(
                                256,
                            ))),
                            conversation_star_sync: ConversationStarSyncGate::default(),
                            user_status_sync: UserStatusSync::default(),
                            team_id: auth.team_id.clone(),
                            huddles,
                            #[cfg(test)]
                            cached_bootstrap_load_gate: None,
                        };
                        runtime_state.connection = Some(connection.clone());
                        connection
                    };

                    events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                        WorkspaceLifecycleEvent::Authenticated,
                    ));
                    events.send_event(RuntimeEventKind::Authenticated(auth));
                    spawn_workspace_tasks(
                        &state_for_task,
                        identity,
                        events,
                        connection,
                        huddle_receiver,
                    );
                }
                Err(error) => {
                    let failure = authentication_failure(failure_context, &error);
                    send_lifecycle_failure_with(&events, &error, failure);
                }
            }
        },
    );
}

fn spawn_workspace_tasks(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    huddle_receiver: mpsc::UnboundedReceiver<HuddleActorMessage>,
) {
    let huddle_events = events.unsolicited(OperationContext::new(
        RuntimeOperation::Huddle,
        RuntimeTarget::Huddle("active".to_string()),
    ));
    spawn_session_task(
        state,
        identity.session,
        run_huddle_actor(
            huddle_receiver,
            huddle_events,
            production_native_join_capability(connection.slack.browser_cookie_d().is_some()),
        ),
    );

    let (hydration_ready_sender, hydration_ready_receiver) = oneshot::channel();
    let state_after_hydration = Arc::clone(state);
    let hydration_events = events.clone();
    let hydration_connection = connection.clone();
    spawn_session_task(state, identity.session, async move {
        if let Some(store) = hydration_connection.workspace_store.as_ref() {
            if let Err(error) = store.ensure_workspace_identity().await {
                crate::debug::log(
                    "store",
                    &format!("WorkspaceIdentityStoreFailed error={error:#}"),
                );
            }
        }
        load_cached_bootstrap(&hydration_events, &hydration_connection).await;
        let _ = hydration_ready_sender.send(());

        let emoji_events = hydration_events.with_context(OperationContext::new(
            RuntimeOperation::Emoji,
            RuntimeTarget::Workspace,
        ));
        let emoji_connection = hydration_connection.clone();
        let emoji_work = RuntimeSyncWork::new(move |_attempt| {
            let emoji_events = emoji_events.clone();
            let emoji_connection = emoji_connection.clone();
            async move {
                match emoji_connection.slack.custom_emojis().await {
                    Ok(emojis) => {
                        if let Some(store) = emoji_connection.workspace_store.as_ref() {
                            if let Err(error) = store.store_custom_emojis(&emojis).await {
                                crate::debug::log(
                                    "store",
                                    &format!(
                                        "CustomEmojiStoreFailed category={:?}",
                                        error.category()
                                    ),
                                );
                            }
                        }
                        emoji_events.send_event(RuntimeEventKind::EmojiCatalogLoaded(emojis));
                        JobOutcome::Succeeded
                    }
                    Err(error) => {
                        crate::debug::log(
                            "runtime",
                            &format!("CustomEmojiRefreshFailed category={:?}", error.category()),
                        );
                        JobOutcome::PermanentFailure
                    }
                }
            }
        });
        schedule_session_sync_work(
            &state_after_hydration,
            identity.session,
            startup_sync_plan(RuntimeStartupSyncKind::EmojiCatalog),
            emoji_work,
        );

        let refresh_events = hydration_events.with_context(OperationContext::new(
            RuntimeOperation::Conversations,
            RuntimeTarget::Workspace,
        ));
        let refresh_connection = hydration_connection.clone();
        let refresh_work = RuntimeSyncWork::new(move |_attempt| {
            let refresh_events = refresh_events.clone();
            let refresh_connection = refresh_connection.clone();
            async move {
                let cached_user_names = refresh_connection
                    .user_cache
                    .lock()
                    .expect("runtime user cache lock poisoned")
                    .clone();
                match load_conversations_best_effort_with_api(
                    &refresh_events,
                    &refresh_connection.slack,
                    refresh_connection.workspace_url.as_deref(),
                    WorkspacePipelineContext {
                        store: &refresh_connection.workspace_store,
                        reducer: &refresh_connection.workspace,
                        conversation_star_sync: &refresh_connection.conversation_star_sync,
                    },
                    cached_user_names,
                    refresh_connection.team_id.as_deref(),
                    &refresh_connection.huddles,
                )
                .await
                {
                    Ok(()) => JobOutcome::Succeeded,
                    Err(error) => {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "ConversationsBackgroundRefreshFailed category={:?}",
                                RuntimeFailure::from_error(&error).category
                            ),
                        );
                        JobOutcome::PermanentFailure
                    }
                }
            }
        });
        schedule_session_sync_work(
            &state_after_hydration,
            identity.session,
            startup_sync_plan(RuntimeStartupSyncKind::Membership),
            refresh_work,
        );

        let group_events = hydration_events.with_context(OperationContext::new(
            RuntimeOperation::User,
            RuntimeTarget::Workspace,
        ));
        let group_connection = hydration_connection;
        let group_work = RuntimeSyncWork::new(move |_attempt| {
            let group_events = group_events.clone();
            let group_connection = group_connection.clone();
            async move {
                let cached_user_names = group_connection
                    .user_cache
                    .lock()
                    .expect("runtime user cache lock poisoned")
                    .clone();
                match load_user_groups_best_effort_with_api(
                    &group_events,
                    &group_connection.slack,
                    &group_connection.workspace_store,
                    cached_user_names,
                )
                .await
                {
                    Ok(()) => JobOutcome::Succeeded,
                    Err(error) => {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "UserGroupsLoadFailed category={:?}",
                                RuntimeFailure::from_error(&error).category
                            ),
                        );
                        JobOutcome::PermanentFailure
                    }
                }
            }
        });
        schedule_session_sync_work(
            &state_after_hydration,
            identity.session,
            startup_sync_plan(RuntimeStartupSyncKind::UserGroups),
            group_work,
        );
    });

    let socket_events = events.unsolicited(OperationContext::new(
        RuntimeOperation::SocketMode,
        RuntimeTarget::Workspace,
    ));
    let browser_credentials = connection.slack.browser_cookie_d().map(|cookie| {
        socket_mode::SocketModeCredentials::BrowserSession {
            xoxc_token: connection.slack.access_token().to_string(),
            xoxd_token: cookie.to_string(),
            user_agent: connection.slack.user_agent().map(str::to_string),
        }
    });
    let credentials = select_realtime_credentials(browser_credentials, configured_app_token);
    match credentials {
        Ok(Some(credentials)) => {
            socket_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                RealtimeStatus::connecting(credentials.transport()),
            ));
            let socket_state = Arc::clone(state);
            let socket_session = identity.session;
            spawn_socket_mode_supervisor(
                state,
                identity.session,
                move |mut cancellation| async move {
                    let hydrated = {
                        let cancellation_wait =
                            std::pin::pin!(wait_for_socket_mode_cancellation(&mut cancellation));
                        let hydration = std::pin::pin!(hydration_ready_receiver);
                        match futures_util::future::select(cancellation_wait, hydration).await {
                            futures_util::future::Either::Left(((), pending_hydration)) => {
                                drop(pending_hydration);
                                false
                            }
                            futures_util::future::Either::Right((ready, pending_cancellation)) => {
                                drop(pending_cancellation);
                                ready.is_ok()
                            }
                        }
                    };
                    if hydrated {
                        run_socket_mode(
                            credentials,
                            socket_events,
                            connection,
                            socket_state,
                            socket_session,
                            cancellation,
                        )
                        .await;
                    }
                },
            );
        }
        Ok(None) => socket_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
            RealtimeStatus::default(),
        )),
        Err(error) => {
            crate::debug::log(
                "socket",
                &format!("SocketModeTokenLoadFailed error={error:#}"),
            );
            socket_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                RealtimeStatus::configuration_error(),
            ));
        }
    }
}

async fn load_cached_bootstrap(events: &RuntimeEventSender, connection: &RuntimeConnection) {
    let Some(store) = connection.workspace_store.as_ref() else {
        return;
    };
    let _admission = connection.workspace.publication_admission.lock().await;
    #[cfg(test)]
    if let Some(gate) = connection.cached_bootstrap_load_gate.as_ref() {
        gate.wait_before_send();
    }
    let bootstrap = match store.load_bootstrap().await {
        Ok(Some(bootstrap)) => bootstrap,
        Ok(None) => return,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!("WorkspaceBootstrapLoadFailed error={error:#}"),
            );
            return;
        }
    };

    let WorkspaceBootstrap {
        workspace_id,
        conversations,
        user_names,
        user_full_names,
        user_avatar_urls,
        user_search_aliases,
        user_statuses,
        thread_catalog,
        custom_emojis,
        reaction_actor_states,
        ..
    } = bootstrap;
    crate::debug::log(
        "runtime",
        &format!(
            "WorkspaceBootstrapLoaded identified={} conversations={} users={} threads={}",
            !workspace_id.is_empty(),
            conversations.len(),
            user_names.len(),
            thread_catalog.len()
        ),
    );
    if !user_names.is_empty() {
        connection
            .user_cache
            .lock()
            .expect("runtime user cache lock poisoned")
            .extend(user_names.clone());
        events.send_event(RuntimeEventKind::UserNamesLoaded(user_names));
    }
    if !user_full_names.is_empty() {
        events.send_event(RuntimeEventKind::UserFullNamesLoaded(user_full_names));
    }
    if !user_avatar_urls.is_empty() {
        events.send_event(RuntimeEventKind::UserAvatarUrlsLoaded(user_avatar_urls));
    }
    if !user_search_aliases.is_empty() {
        events.send_event(RuntimeEventKind::UserSearchAliasesLoaded(
            user_search_aliases,
        ));
    }
    if !user_statuses.is_empty() {
        events.send_event(RuntimeEventKind::UserStatusesLoaded {
            statuses: user_statuses,
            replace_existing: false,
            preserve_user_ids: HashSet::new(),
        });
    }
    let persisted = connection
        .workspace
        .apply_persisted_admitted(
            Some(store),
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations,
                threads: thread_catalog,
                reaction_actor_states,
                ..Default::default()
            }),
        )
        .await;
    match persisted {
        Ok(publication) => {
            for write in publication.writes() {
                publish_persisted_workspace_write(events, write);
            }
        }
        Err(error) => {
            crate::debug::log(
                "store",
                &format!("WorkspaceBootstrapPublishFailed error={error:#}"),
            );
        }
    }
    if !custom_emojis.is_empty() {
        events.send_event(RuntimeEventKind::EmojiCatalogLoaded(custom_emojis));
    }
}

fn select_realtime_credentials(
    browser_credentials: Option<socket_mode::SocketModeCredentials>,
    load_app_token: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<socket_mode::SocketModeCredentials>> {
    match browser_credentials {
        Some(credentials) => Ok(Some(credentials)),
        None => {
            load_app_token().map(|token| token.map(socket_mode::SocketModeCredentials::AppToken))
        }
    }
}

fn user_statuses(users: &[SlackUser]) -> HashMap<String, SlackUserStatus> {
    users
        .iter()
        .filter_map(|user| Some((user.id.clone()?, user.status()?)))
        .collect()
}

fn user_avatar_urls(users: &[SlackUser]) -> HashMap<String, String> {
    users
        .iter()
        .filter_map(|user| Some((user.id.clone()?, user.avatar_url()?)))
        .collect()
}

async fn handle_connected_command(
    command: RuntimeCommand,
    connection: RuntimeConnection,
    events: &RuntimeEventSender,
    image_cache: &ImageAssetCache,
) -> Result<()> {
    let command = match command {
        RuntimeCommand::Huddle(command) => return connection.huddles.command(command),
        command => command,
    };
    let mut slack = Some(connection.slack.clone());
    let mut workspace_store = connection.workspace_store.clone();
    let mut user_cache = connection
        .user_cache
        .lock()
        .expect("runtime user cache lock poisoned")
        .clone();
    let mut read_marks = connection
        .read_marks
        .lock()
        .expect("runtime read marks lock poisoned")
        .clone();
    let mut context = RuntimeContext {
        events,
        image_cache,
        slack: &mut slack,
        workspace_store: &mut workspace_store,
        workspace: &connection.workspace,
        current_user_id: connection.current_user_id.as_deref(),
        user_cache: &mut user_cache,
        read_marks: &mut read_marks,
        message_handoffs: &connection.message_handoffs,
        conversation_star_sync: &connection.conversation_star_sync,
        user_status_sync: &connection.user_status_sync,
        team_id: connection.team_id.as_deref(),
        workspace_url: connection.workspace_url.as_deref(),
        huddles: &connection.huddles,
    };

    let result = handle_command(command, &mut context).await;
    connection
        .user_cache
        .lock()
        .expect("runtime user cache lock poisoned")
        .extend(user_cache);
    let mut shared_read_marks = connection
        .read_marks
        .lock()
        .expect("runtime read marks lock poisoned");
    for (channel_id, timestamp) in read_marks {
        let marked_timestamp = shared_read_marks.entry(channel_id).or_default();
        if timestamp > *marked_timestamp {
            *marked_timestamp = timestamp;
        }
    }
    result
}

struct RuntimeContext<'a> {
    events: &'a RuntimeEventSender,
    image_cache: &'a ImageAssetCache,
    slack: &'a mut Option<SlackApi>,
    workspace_store: &'a mut Option<WorkspaceStore>,
    workspace: &'a WorkspaceReducerAdapter,
    current_user_id: Option<&'a str>,
    user_cache: &'a mut HashMap<String, String>,
    read_marks: &'a mut HashMap<String, String>,
    message_handoffs: &'a Arc<Mutex<MessageHandoffResolver>>,
    conversation_star_sync: &'a ConversationStarSyncGate,
    user_status_sync: &'a UserStatusSync,
    team_id: Option<&'a str>,
    workspace_url: Option<&'a str>,
    huddles: &'a HuddleActorHandle,
}

#[derive(Clone, Copy)]
struct WorkspacePipelineContext<'a> {
    store: &'a Option<WorkspaceStore>,
    reducer: &'a WorkspaceReducerAdapter,
    conversation_star_sync: &'a ConversationStarSyncGate,
}

fn cached_conversation_user_ids(
    conversations: &[SlackConversation],
    user_cache: &HashMap<String, String>,
) -> Vec<String> {
    let mut user_ids = conversations
        .iter()
        .flat_map(SlackConversation::display_user_ids)
        .filter(|user_id| user_cache.contains_key(user_id))
        .collect::<Vec<_>>();
    user_ids.sort();
    user_ids.dedup();
    user_ids
}

#[derive(Debug, Clone)]
struct ChannelHistoryPrefetchCandidate {
    id: String,
    huddle_metadata: bool,
    unread: bool,
    direct_message: bool,
    unread_count: u64,
    activity_score: f64,
    title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConversationUnreadRefreshTier {
    UnknownActiveDirectMessage,
    UnknownDirectMessage,
    UnreadDirectMessage,
    ActiveDirectMessage,
    AttentionOrUnknown,
    Background,
}

#[derive(Debug, Clone)]
struct ConversationUnreadRefreshCandidate {
    id: String,
    tier: ConversationUnreadRefreshTier,
    unread_count: u64,
    priority: f64,
    activity_score: f64,
    title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConversationUnreadRefreshPlan {
    batch: Vec<String>,
    queue: Vec<String>,
    next_queue: Vec<String>,
}

#[cfg(test)]
fn channel_history_prefetch_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    channel_history_prefetch_candidates_with_huddles(conversations, &HashSet::new())
}

fn channel_history_prefetch_candidates_with_huddles(
    conversations: &[SlackConversation],
    current_huddle_channels: &HashSet<String>,
) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter_map(|conversation| {
            channel_history_prefetch_candidate(
                conversation,
                current_huddle_channels.contains(&conversation.id),
            )
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .huddle_metadata
            .cmp(&left.huddle_metadata)
            .then_with(|| right.unread.cmp(&left.unread))
            .then_with(|| right.unread_count.cmp(&left.unread_count))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    let (mut huddle_candidates, remaining): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|candidate| candidate.huddle_metadata);
    let (urgent_direct_messages, mut remaining): (Vec<_>, Vec<_>) = remaining
        .into_iter()
        .partition(|candidate| candidate.unread && candidate.direct_message);
    huddle_candidates.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT);
    remaining.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT.saturating_sub(huddle_candidates.len()));
    huddle_candidates
        .into_iter()
        .chain(urgent_direct_messages)
        .chain(remaining)
        .map(|candidate| candidate.id)
        .collect()
}

fn conversation_unread_refresh_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| !conversation.id.trim().is_empty())
        .map(conversation_unread_refresh_candidate)
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| right.unread_count.cmp(&left.unread_count))
            .then_with(|| right.priority.total_cmp(&left.priority))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.id.clone()))
        .map(|candidate| candidate.id)
        .collect()
}

fn conversation_unread_refresh_candidate(
    conversation: &SlackConversation,
) -> ConversationUnreadRefreshCandidate {
    let unread = conversation.raw_unread_state();
    let direct_message = conversation.is_direct_message();
    let priority = conversation.priority_hint();
    let active_direct_message = conversation.has_active_direct_message_hint();
    let tier = if direct_message && !unread.known && active_direct_message {
        ConversationUnreadRefreshTier::UnknownActiveDirectMessage
    } else if direct_message && !unread.known {
        ConversationUnreadRefreshTier::UnknownDirectMessage
    } else if direct_message && unread.has_unread {
        ConversationUnreadRefreshTier::UnreadDirectMessage
    } else if active_direct_message {
        ConversationUnreadRefreshTier::ActiveDirectMessage
    } else if unread.has_unread || !unread.known {
        ConversationUnreadRefreshTier::AttentionOrUnknown
    } else {
        ConversationUnreadRefreshTier::Background
    };

    ConversationUnreadRefreshCandidate {
        id: conversation.id.clone(),
        tier,
        unread_count: conversation.raw_unread_activity_count(),
        priority,
        activity_score: conversation_activity_score(conversation),
        title: conversation.display_name().to_lowercase(),
    }
}

fn conversation_unread_refresh_plan(
    cached_pending: Vec<String>,
    ranked_candidates: Vec<String>,
    limit: usize,
) -> ConversationUnreadRefreshPlan {
    let mut candidate_ids = HashSet::new();
    let ranked_candidates = ranked_candidates
        .into_iter()
        .filter(|channel_id| !channel_id.trim().is_empty())
        .filter(|channel_id| candidate_ids.insert(channel_id.clone()))
        .collect::<Vec<_>>();
    let mut cached_ids = HashSet::new();
    let cached_pending = cached_pending
        .into_iter()
        .filter(|channel_id| candidate_ids.contains(channel_id))
        .filter(|channel_id| cached_ids.insert(channel_id.clone()))
        .collect::<Vec<_>>();
    // Preserve the circular order for already-queued IDs. Newly unread or active
    // conversations are surfaced by their state/realtime updates, while moving
    // existing IDs back to the front here could indefinitely starve the tail.
    let mut seen = HashSet::new();
    let queue = ranked_candidates
        .into_iter()
        .filter(|channel_id| !cached_ids.contains(channel_id))
        .chain(cached_pending)
        .filter(|channel_id| seen.insert(channel_id.clone()))
        .collect::<Vec<_>>();
    let split = limit.min(queue.len());
    let batch = queue[..split].to_vec();
    let next_queue = queue[split..]
        .iter()
        .chain(&queue[..split])
        .cloned()
        .collect();

    ConversationUnreadRefreshPlan {
        batch,
        queue,
        next_queue,
    }
}

fn channel_history_prefetch_candidate(
    conversation: &SlackConversation,
    current_huddle: bool,
) -> Option<ChannelHistoryPrefetchCandidate> {
    if conversation.is_archived.unwrap_or(false) {
        return None;
    }

    let is_channel = conversation.is_channel.unwrap_or(false)
        || conversation.is_group.unwrap_or(false)
        || conversation.is_private.unwrap_or(false)
        || conversation.is_im.unwrap_or(false)
        || conversation.is_mpim.unwrap_or(false);
    let huddle_metadata = current_huddle || conversation.has_huddle_metadata();
    if !is_channel
        || ((conversation.is_im.unwrap_or(false) || conversation.is_mpim.unwrap_or(false))
            && !conversation.raw_has_unread_activity()
            && !huddle_metadata)
    {
        return None;
    }

    Some(ChannelHistoryPrefetchCandidate {
        id: conversation.id.clone(),
        huddle_metadata,
        unread: conversation.raw_has_unread_activity(),
        direct_message: conversation.is_im.unwrap_or(false)
            || conversation.is_mpim.unwrap_or(false),
        unread_count: conversation.raw_unread_activity_count(),
        activity_score: conversation_activity_score(conversation),
        title: conversation.display_name().to_lowercase(),
    })
}

fn conversation_activity_score(conversation: &SlackConversation) -> f64 {
    [
        "last_read",
        "updated",
        "updated_at",
        "created",
        "latest",
        "latest_ts",
    ]
    .into_iter()
    .filter_map(|key| conversation.extra.get(key).and_then(slack_numeric_value))
    .fold(0.0, f64::max)
}

fn slack_numeric_value(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

async fn handle_command(command: RuntimeCommand, context: &mut RuntimeContext<'_>) -> Result<()> {
    match command {
        RuntimeCommand::LoadStoredToken
        | RuntimeCommand::StartOAuth { .. }
        | RuntimeCommand::StartBrowserSession { .. }
        | RuntimeCommand::SignOut
        | RuntimeCommand::Disconnect
        | RuntimeCommand::UpdateAttentionPreferences(_) => {
            return Err(anyhow!("session command reached connected task handler"));
        }
        RuntimeCommand::Huddle(_) => {
            return Err(anyhow!("huddle command reached generic task handler"));
        }
        RuntimeCommand::RefreshConversations => {
            crate::debug::log("runtime", "RefreshConversations");
            context
                .events
                .send_event(RuntimeEventKind::WorkspaceLifecycle(
                    WorkspaceLifecycleEvent::RecoveryStarted,
                ));
            let api = require_slack(context.slack)?.clone();
            let workspace_store = (*context.workspace_store).clone();
            let cached_user_names = context.user_cache.clone();
            load_conversations_best_effort_with_api(
                context.events,
                &api,
                context.workspace_url,
                WorkspacePipelineContext {
                    store: &workspace_store,
                    reducer: context.workspace,
                    conversation_star_sync: context.conversation_star_sync,
                },
                cached_user_names,
                context.team_id,
                context.huddles,
            )
            .await?;
        }
        RuntimeCommand::DiscoverConversations => {
            let api = require_slack(context.slack)?;
            let status_base_revision = context.user_status_sync.revision();
            let users_base_revision = context.workspace.revision();
            let users = api.users().await?;
            context.workspace.apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    users_base_revision,
                    users.clone(),
                )),
            );
            let aliases = users
                .iter()
                .filter_map(|user| Some((user.id.clone()?, user.search_aliases())))
                .collect::<HashMap<_, _>>();
            let full_names = users
                .iter()
                .filter_map(|user| Some((user.id.clone()?, user.full_name()?)))
                .collect::<HashMap<_, _>>();
            let avatar_urls = user_avatar_urls(&users);
            let statuses = user_statuses(&users);
            if let Some(store) = context.workspace_store.as_ref() {
                store.store_user_search_aliases(&aliases).await?;
                store.store_user_full_names(&full_names).await?;
                store.store_user_avatar_urls(&avatar_urls).await?;
                let _persistence_guard = context.user_status_sync.persistence.lock().await;
                if context
                    .user_status_sync
                    .is_revision_current(status_base_revision)
                {
                    store.store_user_statuses(&statuses).await?;
                }
            }
            context
                .events
                .send_event(RuntimeEventKind::UserSearchAliasesLoaded(aliases));
            context
                .events
                .send_event(RuntimeEventKind::UserFullNamesLoaded(full_names));
            context
                .events
                .send_event(RuntimeEventKind::UserAvatarUrlsLoaded(avatar_urls));
            context
                .user_status_sync
                .publish_snapshot(status_base_revision, |preserve_user_ids| {
                    context
                        .events
                        .send_event(RuntimeEventKind::UserStatusesLoaded {
                            statuses,
                            replace_existing: true,
                            preserve_user_ids,
                        });
                });
            context
                .events
                .send_event(RuntimeEventKind::ConversationPeopleDiscovered(users));
            let channels = api.discover_conversations().await?;
            context
                .events
                .send_event(RuntimeEventKind::ConversationChannelsDiscovered(channels));
        }
        RuntimeCommand::DiscoverChannels => {
            let api = require_slack(context.slack)?;
            let channels = api.discover_conversations().await?;
            context
                .events
                .send_event(RuntimeEventKind::ConversationChannelsDiscovered(channels));
        }
        RuntimeCommand::JoinConversation { channel_id } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Joining conversation");
            let conversation = api.join_conversation(&channel_id).await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation),
                    RuntimeEventKind::ConversationOpened { channel_id },
                )
                .await?;
        }
        RuntimeCommand::LeaveConversation { channel_id } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Leaving channel");
            api.leave_conversation(&channel_id).await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationRemove {
                        channel_id: channel_id.clone(),
                    },
                    RuntimeEventKind::ConversationLeft { channel_id },
                )
                .await?;
        }
        RuntimeCommand::OpenDirectMessage { user_id } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Opening direct message");
            let mut conversation = api.open_direct_message(&user_id).await?;
            conversation.user = Some(user_id);
            conversation.is_im = Some(true);
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    RuntimeEventKind::ConversationOpened {
                        channel_id: conversation.id.clone(),
                    },
                )
                .await?;
        }
        RuntimeCommand::OpenGroupDirectMessage { user_ids } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Opening group DM");
            let mut conversation = api.open_direct_message_with_users(&user_ids).await?;
            if conversation.group_direct_message_user_ids().is_empty() {
                conversation
                    .extra
                    .insert("users".to_string(), serde_json::json!(user_ids));
            }
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    RuntimeEventKind::ConversationOpened {
                        channel_id: conversation.id.clone(),
                    },
                )
                .await?;
        }
        RuntimeCommand::CreateChannel { name, is_private } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Creating channel");
            let conversation = api.create_channel(&name, is_private).await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    RuntimeEventKind::ConversationOpened {
                        channel_id: conversation.id.clone(),
                    },
                )
                .await?;
        }
        RuntimeCommand::InviteToChannel {
            channel_id,
            user_ids,
        } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Adding people");
            let conversation = api.invite_to_channel(&channel_id, &user_ids).await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    RuntimeEventKind::ConversationUpdated { channel_id },
                )
                .await?;
        }
        RuntimeCommand::LoadHistory { channel_id } => {
            let api = require_slack(context.slack)?;
            crate::debug::log("runtime", &format!("LoadHistory channel_id={channel_id}"));
            let service = ConversationHistoryService::new(api, context.workspace_store.as_ref());
            match service.load_cached(&channel_id).await {
                Ok(Some(messages)) if !messages.is_empty() => {
                    observe_huddle_messages(
                        context.huddles,
                        context.team_id,
                        &channel_id,
                        &messages,
                    );
                    if let Err(error) = publish_history_snapshot_with_completion(
                        context.events,
                        context.workspace_store,
                        context.workspace,
                        &channel_id,
                        MutationOrigin::Cache,
                        WorkspaceRevision::INITIAL,
                        messages,
                        false,
                        None,
                        true,
                        false,
                        true,
                    )
                    .await
                    {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "CachedHistoryStoreFailed channel_id={channel_id} category={:?}",
                                error.category()
                            ),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => crate::debug::log(
                    "runtime",
                    &format!(
                        "CachedHistoryLoadFailed channel_id={channel_id} category={:?}",
                        error.category()
                    ),
                ),
            }
            context.events.send_status("Loading conversation");
            let base_revision = context.workspace.revision();
            let page = service.fetch(&channel_id).await?;
            observe_huddle_messages(
                context.huddles,
                context.team_id,
                &channel_id,
                &page.messages,
            );
            crate::debug::log(
                "runtime",
                &format!(
                    "HistoryLoaded channel_id={channel_id} messages={} has_more={} next_cursor={}",
                    page.messages.len(),
                    page.has_more,
                    page.next_cursor.is_some()
                ),
            );
            let complete = !page.has_more && page.next_cursor.is_none();
            publish_history_snapshot_with_completion(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                MutationOrigin::WebApi,
                base_revision,
                page.messages,
                page.has_more,
                page.next_cursor,
                complete,
                false,
                false,
            )
            .await?;
        }
        RuntimeCommand::LoadOlderHistory { channel_id, cursor } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadOlderHistory channel_id={channel_id}"),
            );
            context.events.send_status("Loading older messages");
            let base_revision = context.workspace.revision();
            let page = api.history_page(&channel_id, Some(&cursor)).await?;
            observe_huddle_messages(
                context.huddles,
                context.team_id,
                &channel_id,
                &page.messages,
            );
            publish_history_snapshot_with_completion(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                MutationOrigin::WebApi,
                base_revision,
                page.messages,
                page.has_more,
                page.next_cursor,
                false,
                true,
                false,
            )
            .await?;
        }
        RuntimeCommand::LoadThread { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            context.workspace.clear_thread_paging();
            load_cached_thread(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                &ts,
            )
            .await;
            context.events.send_status("Loading thread");
            let base_revision = context.workspace.begin_thread_fetch(&channel_id, &ts);
            let page = match api.thread_replies(&channel_id, &ts).await {
                Ok(page) => page,
                Err(error) => {
                    context
                        .workspace
                        .clear_matching_thread_fetch(&channel_id, &ts);
                    return Err(error.into());
                }
            };
            let complete = thread_page_is_complete(page.has_more, page.next_cursor.as_deref());
            context.workspace.record_initial_thread_page(
                &channel_id,
                &ts,
                &page.messages,
                complete,
            );
            publish_thread_snapshot_with_completion(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                &ts,
                MutationOrigin::WebApi,
                base_revision,
                page,
                false,
            )
            .await?;
        }
        RuntimeCommand::LoadOlderThread {
            channel_id,
            ts,
            cursor,
        } => {
            let api = require_slack(context.slack)?;
            let fallback_base_revision = context.workspace.revision();
            crate::debug::log(
                "runtime",
                &format!("LoadOlderThread channel_id={channel_id} ts={ts}"),
            );
            context.events.send_status("Loading more replies");
            let page = api
                .thread_replies_page(&channel_id, &ts, Some(&cursor))
                .await?;
            let snapshot = context.workspace.older_thread_snapshot_page(
                &channel_id,
                &ts,
                page.messages.clone(),
                page.has_more,
                page.next_cursor.clone(),
                fallback_base_revision,
            );
            let base_revision = snapshot.base_revision();
            let snapshot_page = snapshot.into_data();
            publish_thread_snapshot_page_with_completion(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                &ts,
                MutationOrigin::WebApi,
                base_revision,
                page,
                snapshot_page,
                true,
            )
            .await?;
        }
        RuntimeCommand::LoadMessageContext(location) => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Loading message context");
            let page = if let Some(thread_ts) = location.thread_ts() {
                let page = api
                    .thread_replies_context(location.channel_id(), thread_ts, location.message_ts())
                    .await?;
                page
            } else {
                api.history_context(location.channel_id(), location.message_ts())
                    .await?
            };
            context
                .events
                .send_event(RuntimeEventKind::MessageContextLoaded {
                    location,
                    messages: page.messages,
                });
        }
        RuntimeCommand::SearchMessages { query } => {
            let api = require_slack(context.slack)?;
            let results = api.search_messages(&query).await?;
            context
                .events
                .send_event(RuntimeEventKind::SearchLoaded(results));
        }
        RuntimeCommand::LoadFiles => {
            let api = require_slack(context.slack)?;
            let files = api.files().await?;
            context
                .events
                .send_event(RuntimeEventKind::FilesLoaded(files));
        }
        RuntimeCommand::LoadFile {
            file_id,
            share_requested,
        } => {
            let api = require_slack(context.slack)?;
            let file = api.file(&file_id).await?;
            context.events.send_event(RuntimeEventKind::FileLoaded {
                file: Box::new(file),
                share_requested,
            });
        }
        RuntimeCommand::LoadSavedItems => {
            let api = require_slack(context.slack)?;
            let items = api.saved_items().await?;
            context
                .events
                .send_event(RuntimeEventKind::SavedItemsLoaded(items));
        }
        RuntimeCommand::LoadUser { user_id } => {
            if let Some(display_name) = context.user_cache.get(&user_id).cloned() {
                context.events.send_event(RuntimeEventKind::UserLoaded {
                    user_id,
                    display_name,
                    full_name: None,
                    avatar_url: None,
                    status: None,
                });
            } else {
                let status_base_revision = context.user_status_sync.user_revision(&user_id);
                let api = require_slack(context.slack)?;
                let user = api.user(&user_id).await?;
                let display_name = user.display_name().unwrap_or_else(|| user_id.clone());
                let full_name = user.full_name();
                let avatar_url = user.avatar_url();
                let status = user.status();
                context
                    .user_cache
                    .insert(user_id.clone(), display_name.clone());
                store_user_name(context.workspace_store, &user_id, &display_name).await;
                if let Some(full_name) = full_name.as_deref() {
                    store_user_full_name(context.workspace_store, &user_id, full_name).await;
                }
                if let Some(avatar_url) = avatar_url.as_deref() {
                    store_user_avatar_url(context.workspace_store, &user_id, avatar_url).await;
                }
                if let Some(store) = context.workspace_store.as_ref() {
                    let _persistence_guard = context.user_status_sync.persistence.lock().await;
                    if context
                        .user_status_sync
                        .is_user_revision_current(&user_id, status_base_revision)
                    {
                        if let Err(error) = store.store_user_status(&user_id, status.clone()).await
                        {
                            crate::debug::log(
                                "store",
                                &format!(
                                    "CachedUserStatusStoreFailed user_id={user_id} error={error:#}"
                                ),
                            );
                        }
                    }
                }
                let status_user_id = user_id.clone();
                context.user_status_sync.publish_user_snapshot(
                    &status_user_id,
                    status_base_revision,
                    |status_is_current| {
                        context.events.send_event(RuntimeEventKind::UserLoaded {
                            user_id,
                            display_name,
                            full_name,
                            avatar_url,
                            status: status_is_current.then_some(status).flatten(),
                        });
                    },
                );
            }
        }
        RuntimeCommand::LoadUserProfile { user_id } => {
            let api = require_slack(context.slack)?;
            let mut user = api.user(&user_id).await?;
            match api.user_profile(&user_id).await {
                Ok(profile) => user.profile = Some(profile),
                Err(error) => crate::debug::log(
                    "runtime",
                    &format!("UserProfileFieldsUnavailable user_id={user_id} error={error:#}"),
                ),
            }
            context
                .events
                .send_event(RuntimeEventKind::UserProfileLoaded(Box::new(user)));
        }
        RuntimeCommand::LoadImageAsset { key, url } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadImageAsset key={}", crate::debug::url_for_log(&key)),
            );
            match context.image_cache.load(&key).await {
                Ok(Some(data_uri)) => {
                    crate::debug::log(
                        "runtime",
                        &format!("ImageAssetCacheHit key={}", crate::debug::url_for_log(&key)),
                    );
                    context
                        .events
                        .send_event(RuntimeEventKind::ImageAssetLoaded { key, data_uri });
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => crate::debug::log(
                    "runtime",
                    &format!(
                        "ImageAssetCacheReadFailed key={} error={error:#}",
                        crate::debug::url_for_log(&key)
                    ),
                ),
            }

            match api.download_preview_asset(&url).await {
                Ok(asset) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ImageAssetLoaded key={} mime_type={} bytes={}",
                            crate::debug::url_for_log(&key),
                            asset.mime_type,
                            asset.bytes.len()
                        ),
                    );
                    let data_uri = preview_asset_data_uri(asset);
                    if let Err(error) = context.image_cache.store(&key, &data_uri).await {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "ImageAssetCacheWriteFailed key={} error={error:#}",
                                crate::debug::url_for_log(&key)
                            ),
                        );
                    }
                    context
                        .events
                        .send_event(RuntimeEventKind::ImageAssetLoaded { key, data_uri });
                }
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ImageAssetFailed key={} error={error:#}",
                            crate::debug::url_for_log(&key)
                        ),
                    );
                    context
                        .events
                        .send_event(RuntimeEventKind::ImageAssetFailed { key });
                }
            }
        }
        RuntimeCommand::LoadMedia { url, name } => {
            let api = require_slack(context.slack)?;
            let destination = media_cache_path(&url, &name);
            let media = api.download_media(&url, &destination).await?;
            context.events.send_event(RuntimeEventKind::MediaLoaded {
                url,
                name,
                path: media.path,
                mime_type: media.mime_type,
            });
        }
        RuntimeCommand::DownloadAttachment { url, name } => {
            let api = require_slack(context.slack)?;
            let destination = attachment_cache_path(&url, &name);
            maintain_attachment_cache(Some(destination.clone())).await;
            let progress_events = context.events.clone();
            let attachment = api
                .download_attachment(&url, &destination, move |update| {
                    progress_events.send_event(RuntimeEventKind::AttachmentDownloadProgress {
                        fraction: update.fraction,
                        label: update.label,
                    });
                })
                .await?;
            maintain_attachment_cache(Some(attachment.path.clone())).await;
            context
                .events
                .send_event(RuntimeEventKind::AttachmentDownloaded {
                    url,
                    name,
                    path: attachment.path,
                });
        }
        RuntimeCommand::ResolveMessagePermalink { channel_id, ts } => {
            let workspace_url = context
                .workspace_url
                .ok_or_else(|| anyhow!("Slack workspace URL is not available"))?;
            let target = MessageRef::new(channel_id, ts)?;
            if let Some(handoff) = context
                .message_handoffs
                .lock()
                .expect("message handoff cache lock poisoned")
                .cached(&target)
            {
                context
                    .events
                    .send_event(RuntimeEventKind::MessagePermalinkResolved { handoff });
                return Ok(());
            }
            let api = require_slack(context.slack)?;
            let provider_result = api
                .message_permalink(target.channel_id(), target.timestamp())
                .await
                .map_err(|error| message_handoff_provider_failure(&error));
            let handoff = context
                .message_handoffs
                .lock()
                .expect("message handoff cache lock poisoned")
                .resolve_provider_result(workspace_url, &target, provider_result)?;
            context
                .events
                .send_event(RuntimeEventKind::MessagePermalinkResolved { handoff });
        }
        RuntimeCommand::MarkConversationRead { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            mark_conversation_read_best_effort(
                api,
                context.events,
                context.read_marks,
                context.workspace_store,
                context.workspace,
                &channel_id,
                &ts,
            )
            .await;
        }
        RuntimeCommand::MarkThreadRead {
            channel_id,
            thread_ts,
            ts,
        } => {
            publish_local_thread_read(
                context.events,
                context.workspace_store.as_ref(),
                context.workspace,
                &channel_id,
                &thread_ts,
                &ts,
            )
            .await;
        }
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let mut message = api
                .post_message(&channel_id, &text, thread_ts.as_deref())
                .await?;
            backfill_requested_thread_ts(&mut message, thread_ts.as_deref());
            if message.user.is_none() {
                message.user = context.current_user_id.map(str::to_string);
            }
            publish_local_post_message(
                context.events,
                context.workspace_store.as_ref(),
                context.workspace,
                channel_id,
                message,
            )
            .await;
        }
        RuntimeCommand::SetReaction {
            channel_id,
            ts,
            name,
            add,
            thread_ts,
        } => {
            let current_user_id = context
                .current_user_id
                .filter(|user_id| !user_id.trim().is_empty())
                .ok_or_else(|| anyhow!("current Slack user identity is unavailable"))?
                .to_string();
            let api = require_slack(context.slack)?;
            api.set_reaction(&channel_id, &ts, &name, add).await?;
            persist_confirmed_reaction(
                context.events,
                context.workspace,
                context.workspace_store.as_ref(),
                ReactionMutation {
                    channel_id,
                    message_ts: ts,
                    name,
                    user_id: current_user_id,
                    added: add,
                },
                thread_ts,
            )
            .await;
        }
        RuntimeCommand::SetSaved {
            channel_id,
            ts,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            api.set_saved(&channel_id, &ts, add).await?;
            context.events.send_event(RuntimeEventKind::SavedUpdated {
                channel_id,
                saved: add,
                thread_ts,
            });
        }
        RuntimeCommand::SetConversationStarred {
            channel_id,
            starred,
        } => {
            let _star_sync_guard = context.conversation_star_sync.lock().await;
            let api = require_slack(context.slack)?;
            api.set_conversation_starred(&channel_id, starred).await?;
            persist_confirmed_conversation_star(
                context.events,
                context.workspace,
                context.workspace_store.as_ref(),
                channel_id,
                starred,
            )
            .await?;
        }
        RuntimeCommand::SetCurrentUserStatus { status } => {
            let user_id = context
                .current_user_id
                .ok_or_else(|| anyhow!("current Slack user identity is unavailable"))?
                .to_string();
            let api = require_slack(context.slack)?;
            let profile = api.set_current_user_status(&status).await?;
            let status = profile.status();
            let status_for_event = status.clone();
            let revision = context.user_status_sync.publish_change(&user_id, || {
                context
                    .events
                    .send_event(RuntimeEventKind::CurrentUserStatusUpdated {
                        user_id: user_id.clone(),
                        status: status_for_event,
                    });
            });
            if let Some(store) = context.workspace_store.as_ref() {
                let _persistence_guard = context.user_status_sync.persistence.lock().await;
                if context
                    .user_status_sync
                    .is_user_revision_current(&user_id, revision)
                {
                    if let Err(error) = store.store_user_status(&user_id, status).await {
                        crate::debug::log(
                            "store",
                            &format!(
                                "CurrentUserStatusStoreFailed category={:?}",
                                error.category()
                            ),
                        );
                    }
                }
            }
        }
        RuntimeCommand::UploadFile {
            channel_id,
            thread_ts,
            path,
            initial_comment,
            remove_after_upload,
        } => {
            let _temporary_upload = RemoveFileOnDrop::new(remove_after_upload, &path);
            let api = require_slack(context.slack)?;
            context
                .events
                .send_event(RuntimeEventKind::FileUploadProgress {
                    fraction: 0.05,
                    label: "Preparing upload".to_string(),
                });
            let progress_events = context.events.clone();
            let upload = api
                .upload_file(
                    &channel_id,
                    thread_ts.as_deref(),
                    &path,
                    initial_comment.as_deref(),
                    move |update| {
                        progress_events.send_event(RuntimeEventKind::FileUploadProgress {
                            fraction: update.fraction,
                            label: update.label,
                        });
                    },
                )
                .await;
            let file = upload?;
            let label = file
                .title
                .or(file.name)
                .or(file.id)
                .unwrap_or_else(|| "file".to_string());
            context
                .events
                .send_event(RuntimeEventKind::FileUploaded(label));
        }
    }

    Ok(())
}

fn socket_membership_refresh_required(
    scope: &socket_mode::SocketModeConversationRefreshScope,
    current_user_id: Option<&str>,
) -> bool {
    match scope {
        socket_mode::SocketModeConversationRefreshScope::Workspace => true,
        socket_mode::SocketModeConversationRefreshScope::Membership { user_id, .. } => {
            current_user_id.is_some_and(|current| current == user_id)
        }
    }
}

fn schedule_realtime_membership_refresh(
    state: &Arc<Mutex<RuntimeState>>,
    session: SessionId,
    connection: RuntimeConnection,
    events: RuntimeEventSender,
) {
    let work = RuntimeSyncWork::new(move |_attempt| {
        let connection = connection.clone();
        let events = events.clone();
        async move {
            let cached_user_names = connection
                .user_cache
                .lock()
                .expect("runtime user cache lock poisoned")
                .clone();
            match load_conversations_best_effort_with_api(
                &events,
                &connection.slack,
                connection.workspace_url.as_deref(),
                WorkspacePipelineContext {
                    store: &connection.workspace_store,
                    reducer: &connection.workspace,
                    conversation_star_sync: &connection.conversation_star_sync,
                },
                cached_user_names,
                connection.team_id.as_deref(),
                &connection.huddles,
            )
            .await
            {
                Ok(()) => JobOutcome::Succeeded,
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "RealtimeMembershipRefreshFailed category={:?}",
                            RuntimeFailure::from_error(&error).category
                        ),
                    );
                    JobOutcome::PermanentFailure
                }
            }
        }
    });
    schedule_session_sync_work(
        state,
        session,
        startup_sync_plan(RuntimeStartupSyncKind::Membership),
        work,
    );
}

async fn run_socket_mode(
    credentials: socket_mode::SocketModeCredentials,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    state: Arc<Mutex<RuntimeState>>,
    session: SessionId,
    mut cancellation: watch::Receiver<bool>,
) {
    let membership_connection = connection.clone();
    let membership_events = events.with_context(OperationContext::new(
        RuntimeOperation::Conversations,
        RuntimeTarget::Workspace,
    ));
    let RuntimeConnection {
        workspace_store,
        workspace,
        current_user_id,
        team_id,
        huddles,
        user_status_sync,
        ..
    } = connection;
    let mut reconnect_delay = SOCKET_MODE_INITIAL_RECONNECT_DELAY;
    let transport = credentials.transport();

    loop {
        if *cancellation.borrow() {
            return;
        }
        let events_for_run = events.clone();
        let connected_events = events.clone();
        let mut persistence_tasks = tokio::task::JoinSet::new();
        // The socket callback is synchronous, so awaiting publication admission
        // would deadlock the transport. One session-scoped ordered actor handles
        // messages and reactions with or without a store and drains before reconnect.
        let (persistence_sender, persistence_receiver) =
            realtime_persistence_channel(workspace.attention_metrics_handle());
        persistence_tasks.spawn(persist_realtime_events(
            persistence_receiver,
            workspace_store.clone(),
            current_user_id.clone(),
            events_for_run.clone(),
            workspace.clone(),
            user_status_sync.clone(),
        ));
        let persistence_fallback = RealtimePersistenceFallback::new(
            workspace_store.clone(),
            current_user_id.clone(),
            events_for_run.clone(),
            workspace.clone(),
            user_status_sync.clone(),
        );
        let persistence_for_run = persistence_sender.clone();
        let fallback_for_run = persistence_fallback.clone();
        let has_store_for_run = workspace_store.is_some();
        let workspace_for_run = workspace.clone();
        let huddles_for_run = huddles.clone();
        let team_id_for_run = team_id.clone();
        let user_status_sync_for_run = user_status_sync.clone();
        let current_user_id_for_run = current_user_id.clone();
        let membership_connection_for_run = membership_connection.clone();
        let membership_events_for_run = membership_events.clone();
        let membership_state_for_run = Arc::clone(&state);
        let run_once = socket_mode::run_once(
            &credentials,
            move || {
                connected_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                    RealtimeStatus::online(transport),
                ));
            },
            move |event| {
                observe_huddle_socket_event(&huddles_for_run, team_id_for_run.as_deref(), &event);
                let membership_refresh = match &event {
                    SocketModeEvent::RefreshConversations(scope) => {
                        socket_membership_refresh_required(
                            scope,
                            current_user_id_for_run.as_deref(),
                        )
                    }
                    _ => false,
                };
                if membership_refresh {
                    schedule_realtime_membership_refresh(
                        &membership_state_for_run,
                        session,
                        membership_connection_for_run.clone(),
                        membership_events_for_run.clone(),
                    );
                }
                let forward_to_ui = !matches!(&event, SocketModeEvent::RefreshConversations(_));
                let defer_ordered_ui = matches!(
                    &event,
                    SocketModeEvent::Message(_) | SocketModeEvent::Reaction(_)
                );
                let attention = (forward_to_ui && !defer_ordered_ui)
                    .then(|| apply_realtime_workspace_event(&workspace_for_run, &event))
                    .flatten();
                let status_change_user_id = match &event {
                    SocketModeEvent::UserChanged(user)
                    | SocketModeEvent::UserHuddleChanged(user)
                        if user
                            .profile
                            .as_ref()
                            .is_some_and(|profile| profile.contains_status_fields()) =>
                    {
                        user.id.clone()
                    }
                    _ => None,
                };
                let status_revision = if forward_to_ui && !defer_ordered_ui {
                    if let Some(user_id) = status_change_user_id.as_deref() {
                        Some(user_status_sync_for_run.publish_change(user_id, || {
                            events_for_run.send_event(RuntimeEventKind::SocketModeEvent {
                                event: event.clone(),
                                attention: attention.as_ref().map(|effect| effect.decision.clone()),
                            });
                        }))
                    } else {
                        events_for_run.send_event(RuntimeEventKind::SocketModeEvent {
                            event: event.clone(),
                            attention: attention.as_ref().map(|effect| effect.decision.clone()),
                        });
                        None
                    }
                } else {
                    None
                };
                let persistence_event = match &event {
                    SocketModeEvent::UserChanged(user)
                    | SocketModeEvent::UserHuddleChanged(user)
                        if has_store_for_run =>
                    {
                        Some(RealtimePersistenceEvent::UserChanged {
                            user: user.clone(),
                            status_revision,
                        })
                    }
                    SocketModeEvent::Message(message) => Some(RealtimePersistenceEvent::Message {
                        event: message.clone(),
                    }),
                    SocketModeEvent::Reaction(_) => Some(RealtimePersistenceEvent::OrderedEvent {
                        event: event.clone(),
                    }),
                    SocketModeEvent::UserChanged(_)
                    | SocketModeEvent::UserHuddleChanged(_)
                    | SocketModeEvent::RefreshConversations(_) => None,
                };
                if let Some(persistence_event) = persistence_event {
                    if let Err(returned) = persistence_for_run.send(persistence_event) {
                        crate::debug::log(
                            "store",
                            "RealtimePersistenceQueueRejected reason=worker_closed",
                        );
                        fallback_for_run.schedule(returned.0);
                    }
                }
            },
        );
        let result = {
            let cancellation_wait =
                std::pin::pin!(wait_for_socket_mode_cancellation(&mut cancellation));
            let run_once = std::pin::pin!(run_once);
            match futures_util::future::select(cancellation_wait, run_once).await {
                futures_util::future::Either::Left(((), pending_run)) => {
                    drop(pending_run);
                    None
                }
                futures_util::future::Either::Right((result, pending_cancellation)) => {
                    drop(pending_cancellation);
                    Some(result)
                }
            }
        };
        if result.is_some() {
            events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                RealtimeStatus::reconnecting(transport),
            ));
        }
        drop(persistence_sender);
        while let Some(join_result) = persistence_tasks.join_next().await {
            if let Err(error) = join_result {
                crate::debug::log(
                    "store",
                    &format!("RealtimePersistenceWorkerFailed error={error}"),
                );
            }
        }
        persistence_fallback.drain().await;
        workspace.trace_attention_metrics_snapshot();

        let Some(result) = result else {
            return;
        };
        let timing = match result {
            Ok(SocketModeDisconnect::LinkDisabled) => {
                crate::debug::log(
                    "socket",
                    "SocketModeDisconnected reason=link_disabled; retrying until enabled",
                );
                socket_mode_reconnect_timing(
                    reconnect_delay,
                    Some(SocketModeDisconnect::LinkDisabled),
                )
            }
            Ok(disconnect) => {
                crate::debug::log(
                    "socket",
                    &format!("SocketModeDisconnected reason={disconnect:?}"),
                );
                socket_mode_reconnect_timing(reconnect_delay, Some(disconnect))
            }
            Err(error) => {
                crate::debug::log("socket", &format!("SocketModeError error={error:#}"));
                socket_mode_reconnect_timing(reconnect_delay, None)
            }
        };

        reconnect_delay = timing.next_backoff;
        let cancelled = {
            let cancellation_wait =
                std::pin::pin!(wait_for_socket_mode_cancellation(&mut cancellation));
            let reconnect_sleep = std::pin::pin!(tokio::time::sleep(timing.sleep));
            match futures_util::future::select(cancellation_wait, reconnect_sleep).await {
                futures_util::future::Either::Left(((), pending_sleep)) => {
                    drop(pending_sleep);
                    true
                }
                futures_util::future::Either::Right(((), pending_cancellation)) => {
                    drop(pending_cancellation);
                    false
                }
            }
        };
        if cancelled {
            return;
        }
    }
}

async fn wait_for_socket_mode_cancellation(cancellation: &mut watch::Receiver<bool>) {
    while !*cancellation.borrow() {
        if cancellation.changed().await.is_err() {
            break;
        }
    }
}

async fn publish_realtime_reaction_without_store(
    workspace: &WorkspaceReducerAdapter,
    events: &RuntimeEventSender,
    event: SocketModeEvent,
) {
    let _publication = workspace.publication_admission.lock().await;
    let SocketModeEvent::Reaction(event) = event else {
        return;
    };
    persist_realtime_reaction_admitted(None, events, workspace, event).await;
}

fn apply_realtime_workspace_event(
    workspace: &WorkspaceReducerAdapter,
    event: &SocketModeEvent,
) -> Option<MessageAttentionEffect> {
    let mutation = match event {
        SocketModeEvent::Message(event) => Some(realtime_message_mutation(event, None)),
        SocketModeEvent::Reaction(event) => Some(realtime_reaction_mutation(event)),
        SocketModeEvent::UserChanged(user) => Some(WorkspaceMutation::UserUpsert((**user).clone())),
        SocketModeEvent::UserHuddleChanged(_) | SocketModeEvent::RefreshConversations(_) => None,
    };
    let reduction = workspace.apply(MutationOrigin::Realtime, mutation?)?;
    reduction.effects().iter().find_map(|effect| match effect {
        WorkspaceEffect::MessageAttention(effect) => Some(effect.clone()),
        WorkspaceEffect::ThreadRead(_) => None,
    })
}

fn realtime_reaction_mutation(
    event: &crate::socket_mode::SocketModeReactionEvent,
) -> WorkspaceMutation {
    WorkspaceMutation::ReactionChanged(ReactionMutation {
        channel_id: event.channel_id.clone(),
        message_ts: event.ts.clone(),
        name: event.name.clone(),
        user_id: event.user_id.clone(),
        added: event.added,
    })
}

fn preview_realtime_workspace_attention(
    workspace: &WorkspaceReducerAdapter,
    event: &crate::socket_mode::SocketModeMessageEvent,
) -> Option<MessageAttentionEffect> {
    workspace.preview_message_attention(
        &event.channel_id,
        &event.message,
        message_mutation_kind(event.kind),
        MutationOrigin::Realtime,
    )
}

fn classify_socket_attention(
    workspace: &WorkspaceReducerAdapter,
    current_user_id: Option<&str>,
    event: &crate::socket_mode::SocketModeMessageEvent,
) -> AttentionPersistenceStatus {
    if event.kind != SocketModeMessageKind::Posted
        || event.message.user.as_deref() == current_user_id
        || preview_realtime_workspace_attention(workspace, event).is_none()
    {
        return AttentionPersistenceStatus::NotApplicable;
    }
    let coordinator = workspace
        .coordinator
        .lock()
        .expect("workspace coordinator lock poisoned");
    let Some(conversation) = coordinator.conversation(&event.channel_id) else {
        return AttentionPersistenceStatus::Accepted;
    };
    if conversation.has_observed_attention_message(&event.message.ts) {
        return AttentionPersistenceStatus::AlreadyObserved;
    }
    if coordinator.message_is_at_or_before_read_cursor(&event.channel_id, &event.message.ts) {
        return AttentionPersistenceStatus::AtOrBeforeReadCursor;
    }
    AttentionPersistenceStatus::Accepted
}

fn realtime_message_mutation(
    event: &crate::socket_mode::SocketModeMessageEvent,
    delivery: Option<DeliveryState>,
) -> WorkspaceMutation {
    let channel_id = event.channel_id.clone();
    let message = event.message.clone();
    let kind = message_mutation_kind(event.kind);
    match delivery {
        None => WorkspaceMutation::MessageChanged {
            channel_id,
            message,
            kind,
            origin: MutationOrigin::Realtime,
        },
        Some(delivery) => WorkspaceMutation::MessageChangedWithDelivery {
            channel_id,
            message,
            kind,
            origin: MutationOrigin::Realtime,
            delivery,
        },
    }
}

const fn message_mutation_kind(kind: SocketModeMessageKind) -> MessageMutationKind {
    match kind {
        SocketModeMessageKind::Posted => MessageMutationKind::Posted,
        SocketModeMessageKind::Changed => MessageMutationKind::Changed,
        SocketModeMessageKind::Deleted => MessageMutationKind::Deleted,
    }
}

fn observe_huddle_socket_event(
    huddles: &HuddleActorHandle,
    team_id: Option<&str>,
    event: &SocketModeEvent,
) {
    let result = match event {
        SocketModeEvent::Message(event) => {
            let Some(room) = event.message.room.as_ref() else {
                return;
            };
            if room.has_ended() {
                room.id
                    .as_deref()
                    .map(str::trim)
                    .filter(|call_id| !call_id.is_empty())
                    .map(|call_id| {
                        huddles.input(CoordinatorInput::HuddleEnded {
                            call_id: call_id.to_string(),
                        })
                    })
                    .unwrap_or(Ok(()))
            } else {
                team_id
                    .and_then(|team_id| room.active_huddle(team_id, &event.channel_id))
                    .map(|huddle| huddles.observe_huddle(huddle))
                    .unwrap_or(Ok(()))
            }
        }
        SocketModeEvent::UserHuddleChanged(user) => observe_huddle_user(huddles, user),
        SocketModeEvent::UserChanged(user)
            if user.profile.as_ref().is_some_and(|profile| {
                profile.huddle_state_call_id.is_some()
                    || profile.huddle_state_channel_id.is_some()
                    || profile.huddle_state_expiration_ts.is_some()
                    || profile.huddle_state != crate::huddles::model::SlackHuddleState::DefaultUnset
            }) =>
        {
            observe_huddle_user(huddles, user)
        }
        SocketModeEvent::UserChanged(_)
        | SocketModeEvent::Reaction(_)
        | SocketModeEvent::RefreshConversations(_) => Ok(()),
    };

    if result.is_err() {
        crate::debug::log("huddle", "HuddleRealtimeObservationDropped");
    }
}

fn observe_huddle_messages(
    huddles: &HuddleActorHandle,
    team_id: Option<&str>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(team_id) = team_id.map(str::trim).filter(|team_id| !team_id.is_empty()) else {
        return;
    };
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return;
    }

    let mut room_messages = messages
        .iter()
        .filter(|message| message.room.is_some())
        .collect::<Vec<_>>();
    room_messages.sort_by(|left, right| left.ts.cmp(&right.ts));

    for message in room_messages {
        let room = message.room.as_ref().expect("room message was filtered");
        if !room.channels.is_empty()
            && !room
                .channels
                .iter()
                .any(|candidate| candidate.trim() == channel_id)
        {
            continue;
        }
        let result = if room.has_ended() {
            room.id
                .as_deref()
                .map(str::trim)
                .filter(|call_id| !call_id.is_empty())
                .map(|call_id| {
                    huddles.input(CoordinatorInput::HuddleEnded {
                        call_id: call_id.to_string(),
                    })
                })
                .unwrap_or(Ok(()))
        } else {
            room.active_huddle(team_id, channel_id)
                .map(|huddle| huddles.observe_huddle(huddle))
                .unwrap_or(Ok(()))
        };
        if result.is_err() {
            crate::debug::log("huddle", "HuddleHistoryObservationDropped");
            return;
        }
    }
}

fn observe_huddle_user(huddles: &HuddleActorHandle, user: &SlackUser) -> Result<()> {
    let Some(user_id) = user
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(());
    };
    let unix_seconds = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    let presence =
        HuddlePresence::from_user(user).filter(|presence| presence.is_active_at(unix_seconds));
    huddles.input(CoordinatorInput::PresenceChanged {
        user_id: user_id.to_string(),
        presence,
    })
}

async fn run_huddle_actor(
    mut receiver: mpsc::UnboundedReceiver<HuddleActorMessage>,
    events: RuntimeEventSender,
    join_capability: NativeJoinCapability,
) {
    let mut coordinator = HuddleCoordinator::default();
    let _ = coordinator.apply(CoordinatorInput::JoinCapabilityChanged(
        join_capability.is_available(),
    ));
    while let Some(message) = receiver.recv().await {
        let input = match message {
            HuddleActorMessage::Command(command) => huddle_input_from_command(command),
            HuddleActorMessage::Input(input) => input,
        };
        match coordinator.apply(input) {
            Ok(effects) => {
                apply_huddle_effects(&mut coordinator, effects, &events, join_capability)
            }
            Err(error) => {
                crate::debug::log("huddle", &format!("HuddleTransitionRejected error={error}"))
            }
        }
    }

    let _ = coordinator.apply(CoordinatorInput::Reset);
}

fn huddle_input_from_command(command: HuddleCommand) -> CoordinatorInput {
    match command {
        HuddleCommand::OpenPreflight { call_id } => CoordinatorInput::OpenPreflight { call_id },
        HuddleCommand::Join { call_id } => CoordinatorInput::JoinRequested { call_id },
        HuddleCommand::OpenExternally { call_id } => CoordinatorInput::OpenExternally { call_id },
        HuddleCommand::Leave => CoordinatorInput::LeaveRequested,
        HuddleCommand::Dismiss => CoordinatorInput::Dismissed,
        HuddleCommand::SetMuted(muted) => CoordinatorInput::MutedChanged(muted),
        HuddleCommand::SetCameraEnabled(enabled) => CoordinatorInput::CameraChanged(enabled),
        HuddleCommand::SetScreenShareEnabled(enabled) => {
            CoordinatorInput::ScreenShareChanged(enabled)
        }
        HuddleCommand::SelectDevice { kind, id } => CoordinatorInput::DeviceSelected { kind, id },
    }
}

fn apply_huddle_effects(
    coordinator: &mut HuddleCoordinator,
    effects: Vec<HuddleEffect>,
    events: &RuntimeEventSender,
    join_capability: NativeJoinCapability,
) {
    let mut pending = std::collections::VecDeque::from(effects);
    while let Some(effect) = pending.pop_front() {
        match effect {
            HuddleEffect::Publish(snapshot) => {
                events.send_event(RuntimeEventKind::Huddle(HuddleEvent::Snapshot(Box::new(
                    snapshot,
                ))));
            }
            HuddleEffect::BeginNativeJoin { .. } => {
                let failure = match join_capability {
                    NativeJoinCapability::Unavailable(reason) => reason.failure(),
                    NativeJoinCapability::Available { .. } => HuddleFailure::protocol_changed(),
                };
                if let Ok(effects) = coordinator.apply(CoordinatorInput::Failed(failure)) {
                    pending.extend(effects);
                }
            }
            HuddleEffect::StopSession if coordinator.snapshot().phase == HuddlePhase::Leaving => {
                if let Ok(effects) = coordinator.apply(CoordinatorInput::MediaStopped) {
                    pending.extend(effects);
                }
            }
            HuddleEffect::OpenExternal(huddle) => events.send_event(RuntimeEventKind::Huddle(
                HuddleEvent::OpenExternalRequested(huddle),
            )),
            HuddleEffect::ApplyControls(_)
            | HuddleEffect::ApplyDeviceSelection(_)
            | HuddleEffect::StartScreenShare
            | HuddleEffect::StopScreenShare
            | HuddleEffect::StopSession => {}
        }
    }
}

#[derive(Debug)]
enum RealtimePersistenceEvent {
    UserChanged {
        user: Box<SlackUser>,
        status_revision: Option<u64>,
    },
    Message {
        event: Box<crate::socket_mode::SocketModeMessageEvent>,
    },
    OrderedEvent {
        event: SocketModeEvent,
    },
}

#[derive(Clone, Debug)]
struct RealtimePersistenceSender {
    sender: mpsc::UnboundedSender<RealtimePersistenceEvent>,
    metrics: Arc<AttentionMetrics>,
}

impl RealtimePersistenceSender {
    fn send(
        &self,
        event: RealtimePersistenceEvent,
    ) -> Result<(), mpsc::error::SendError<RealtimePersistenceEvent>> {
        self.metrics.record_queue_send(|| self.sender.send(event))
    }
}

struct RealtimePersistenceReceiver {
    receiver: mpsc::UnboundedReceiver<RealtimePersistenceEvent>,
    metrics: Arc<AttentionMetrics>,
}

impl RealtimePersistenceReceiver {
    async fn recv(&mut self) -> Option<RealtimePersistenceEvent> {
        let event = self.receiver.recv().await;
        if event.is_some() {
            self.metrics.dequeue_queue_slot();
        }
        event
    }
}

fn realtime_persistence_channel(
    metrics: Arc<AttentionMetrics>,
) -> (RealtimePersistenceSender, RealtimePersistenceReceiver) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        RealtimePersistenceSender {
            sender,
            metrics: Arc::clone(&metrics),
        },
        RealtimePersistenceReceiver { receiver, metrics },
    )
}

#[derive(Clone)]
struct RealtimePersistenceFallback {
    store: Option<WorkspaceStore>,
    current_user_id: Option<String>,
    events: RuntimeEventSender,
    workspace: WorkspaceReducerAdapter,
    user_status_sync: UserStatusSync,
    tail: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl RealtimePersistenceFallback {
    fn new(
        store: Option<WorkspaceStore>,
        current_user_id: Option<String>,
        events: RuntimeEventSender,
        workspace: WorkspaceReducerAdapter,
        user_status_sync: UserStatusSync,
    ) -> Self {
        Self {
            store,
            current_user_id,
            events,
            workspace,
            user_status_sync,
            tail: Arc::new(Mutex::new(None)),
        }
    }

    /// Immediately schedules ownership returned by a closed primary actor.
    ///
    /// Each task waits for its predecessor, preserving socket FIFO. Message
    /// persistence still enters the workspace's shared store admission gate,
    /// so this is not an independent writer or publication lane.
    fn schedule(&self, event: RealtimePersistenceEvent) {
        let store = self.store.clone();
        let current_user_id = self.current_user_id.clone();
        let events = self.events.clone();
        let workspace = self.workspace.clone();
        let user_status_sync = self.user_status_sync.clone();
        let mut tail = self
            .tail
            .lock()
            .expect("realtime persistence fallback lock poisoned");
        let predecessor = tail.take();
        *tail = Some(tokio::spawn(async move {
            if let Some(predecessor) = predecessor {
                if let Err(error) = predecessor.await {
                    crate::debug::log(
                        "store",
                        &format!("RealtimePersistenceFallbackFailed error={error}"),
                    );
                }
            }
            persist_realtime_event(
                event,
                store.as_ref(),
                current_user_id.as_deref(),
                &events,
                &workspace,
                &user_status_sync,
            )
            .await;
        }));
    }

    async fn drain(&self) {
        let tail = self
            .tail
            .lock()
            .expect("realtime persistence fallback lock poisoned")
            .take();
        if let Some(tail) = tail {
            if let Err(error) = tail.await {
                crate::debug::log(
                    "store",
                    &format!("RealtimePersistenceFallbackFailed error={error}"),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttentionPersistenceStatus {
    NotApplicable,
    Accepted,
    AlreadyObserved,
    AtOrBeforeReadCursor,
    Failed,
}

impl AttentionPersistenceStatus {
    const fn metrics_outcome(self) -> AttentionPersistenceOutcome {
        match self {
            Self::NotApplicable => AttentionPersistenceOutcome::NotApplicable,
            Self::Accepted => AttentionPersistenceOutcome::Accepted,
            Self::AlreadyObserved => AttentionPersistenceOutcome::AlreadyObserved,
            Self::AtOrBeforeReadCursor => AttentionPersistenceOutcome::AtOrBeforeReadCursor,
            Self::Failed => AttentionPersistenceOutcome::Failed,
        }
    }
}

fn claimed_notification_candidate(
    notification_claimed: bool,
    applied_attention: Option<&MessageAttentionEffect>,
) -> Option<MessageAttentionEffect> {
    notification_claimed
        .then(|| {
            applied_attention
                .filter(|effect| effect.decision.send_notification)
                .cloned()
        })
        .flatten()
}

fn publish_persisted_workspace_write(events: &RuntimeEventSender, write: &PersistedWorkspaceWrite) {
    publish_workspace_reduction(events, &write.reduction);
    publish_persisted_workspace_notification(events, write);
}

fn publish_persisted_workspace_notification(
    events: &RuntimeEventSender,
    write: &PersistedWorkspaceWrite,
) {
    if let Some(notification) = write.notification() {
        // The ledger claim is already durable with the message projection.
        // Abrupt process exit before this native UI delivery is therefore an
        // intentional at-most-once boundary, not a durable notification outbox.
        // Session replacement also drains this attempt before retiring the
        // old supervisor, but the old SessionId remains a privacy boundary:
        // the window may reject its candidate after explicitly moving on.
        events.send_event(RuntimeEventKind::AttentionNotificationCandidate {
            channel_id: notification.channel_id,
            message: Box::new(notification.message),
            decision: notification.decision,
        });
    }
}

async fn persist_realtime_events(
    mut receiver: RealtimePersistenceReceiver,
    store: impl Into<Option<WorkspaceStore>>,
    current_user_id: Option<String>,
    events: RuntimeEventSender,
    workspace: WorkspaceReducerAdapter,
    user_status_sync: UserStatusSync,
) {
    let store = store.into();
    while let Some(event) = receiver.recv().await {
        persist_realtime_event(
            event,
            store.as_ref(),
            current_user_id.as_deref(),
            &events,
            &workspace,
            &user_status_sync,
        )
        .await;
    }
}

async fn persist_realtime_event(
    event: RealtimePersistenceEvent,
    store: Option<&WorkspaceStore>,
    current_user_id: Option<&str>,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    user_status_sync: &UserStatusSync,
) {
    match event {
        RealtimePersistenceEvent::UserChanged {
            user,
            status_revision,
        } => {
            let Some(store) = store else {
                return;
            };
            let Some(user_id) = user.id.as_deref() else {
                return;
            };
            if let Some(full_name) = user.full_name() {
                if let Err(error) = store
                    .store_user_full_names(&HashMap::from([(user_id.to_string(), full_name)]))
                    .await
                {
                    crate::debug::log(
                        "store",
                        &format!(
                            "RealtimeUserFullNameStoreFailed user_id={user_id} error={error:#}"
                        ),
                    );
                }
            }
            if let Some(avatar_url) = user.avatar_url() {
                if let Err(error) = store
                    .store_user_avatar_urls(&HashMap::from([(user_id.to_string(), avatar_url)]))
                    .await
                {
                    crate::debug::log(
                        "store",
                        &format!(
                            "RealtimeUserAvatarUrlStoreFailed user_id={user_id} error={error:#}"
                        ),
                    );
                }
            }
            if user
                .profile
                .as_ref()
                .is_some_and(|profile| profile.contains_status_fields())
            {
                let _persistence_guard = user_status_sync.persistence.lock().await;
                let status_is_current = status_revision.is_none_or(|revision| {
                    user_status_sync.is_user_revision_current(user_id, revision)
                });
                if status_is_current {
                    if let Err(error) = store.store_user_status(user_id, user.status()).await {
                        crate::debug::log(
                            "store",
                            &format!(
                                "RealtimeUserStatusStoreFailed user_id={user_id} error={error:#}"
                            ),
                        );
                    }
                }
            }
        }
        RealtimePersistenceEvent::Message { event } => {
            if let Some(store) = store {
                persist_socket_message(store, current_user_id, events, workspace, *event).await;
            } else {
                publish_socket_message_without_store(current_user_id, events, workspace, *event)
                    .await;
            }
        }
        RealtimePersistenceEvent::OrderedEvent { event } => match event {
            SocketModeEvent::Reaction(event) => {
                if let Some(store) = store {
                    persist_realtime_reaction(store, events, workspace, event).await;
                } else {
                    publish_realtime_reaction_without_store(
                        workspace,
                        events,
                        SocketModeEvent::Reaction(event),
                    )
                    .await;
                }
            }
            event => {
                apply_realtime_workspace_event(workspace, &event);
                events.send_event(RuntimeEventKind::SocketModeEvent {
                    event,
                    attention: None,
                });
            }
        },
    }
}

async fn persist_realtime_reaction(
    store: &WorkspaceStore,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    event: crate::socket_mode::SocketModeReactionEvent,
) {
    let _admission = workspace.publication_admission.lock().await;
    persist_realtime_reaction_admitted(Some(store), events, workspace, event).await;
}

async fn persist_realtime_reaction_admitted(
    store: Option<&WorkspaceStore>,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    event: crate::socket_mode::SocketModeReactionEvent,
) {
    let recovered = match workspace.recover_persisted_admitted(store).await {
        Ok(writes) => writes,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeReactionRecoveryDeferred channel_id={} category={:?}",
                    event.channel_id,
                    error.category()
                ),
            );
            workspace.apply_and_enqueue(
                store,
                MutationOrigin::Realtime,
                realtime_reaction_mutation(&event),
            );
            events.send_event(RuntimeEventKind::SocketModeEvent {
                event: SocketModeEvent::Reaction(event),
                attention: None,
            });
            return;
        }
    };
    for write in recovered.writes() {
        publish_persisted_workspace_write(events, write);
    }
    drop(recovered);

    workspace.apply_and_enqueue(
        store,
        MutationOrigin::Realtime,
        realtime_reaction_mutation(&event),
    );
    let persisted = match workspace.recover_persisted_admitted(store).await {
        Ok(publication) => Some(publication),
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeReactionDeltaDeferred channel_id={} category={:?}",
                    event.channel_id,
                    error.category()
                ),
            );
            None
        }
    };
    events.send_event(RuntimeEventKind::SocketModeEvent {
        event: SocketModeEvent::Reaction(event),
        attention: None,
    });
    if let Some(publication) = persisted {
        for write in publication.writes() {
            publish_persisted_workspace_write(events, write);
        }
    }
}

/// Publish Slack's authoritative response before recoverable local cache work.
///
/// A later sync can restore a cache write interrupted by shutdown, while delaying
/// this event keeps the composer waiting on SQLite after Slack accepted the message.
async fn publish_posted_message_before_persistence<F>(
    events: RuntimeEventSender,
    channel_id: String,
    message: SlackMessage,
    persistence: F,
) where
    F: Future<Output = ()>,
{
    events.send_event(RuntimeEventKind::MessagePosted {
        channel_id,
        message: Box::new(message),
    });
    persistence.await;
}

async fn publish_local_post_message(
    events: &RuntimeEventSender,
    store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: String,
    message: SlackMessage,
) {
    let _admission = workspace.publication_admission.lock().await;
    workspace.apply_and_enqueue(
        store,
        MutationOrigin::Local,
        WorkspaceMutation::MessageChanged {
            channel_id: channel_id.clone(),
            message: message.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Local,
        },
    );

    let persistence_events = events.clone();
    publish_posted_message_before_persistence(events.clone(), channel_id.clone(), message, async {
        persist_and_publish_local_reductions(
            &persistence_events,
            store,
            workspace,
            "LocalPost",
            &channel_id,
        )
        .await;
    })
    .await;
}

fn backfill_requested_thread_ts(message: &mut SlackMessage, requested_thread_ts: Option<&str>) {
    let Some(requested_thread_ts) = requested_thread_ts
        .map(str::trim)
        .filter(|thread_ts| !thread_ts.is_empty())
    else {
        return;
    };
    if message
        .thread_ts
        .as_deref()
        .is_none_or(|thread_ts| thread_ts.trim().is_empty())
    {
        message.thread_ts = Some(requested_thread_ts.to_string());
    }
}

async fn publish_local_thread_read(
    events: &RuntimeEventSender,
    store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    thread_ts: &str,
    ts: &str,
) {
    if channel_id.trim().is_empty() || thread_ts.trim().is_empty() || ts.trim().is_empty() {
        return;
    }

    let _admission = workspace.publication_admission.lock().await;
    workspace.apply_and_enqueue(
        store,
        MutationOrigin::Local,
        WorkspaceMutation::ThreadReadAdvanced {
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
            ts: ts.to_string(),
        },
    );
    persist_and_publish_local_reductions(events, store, workspace, "LocalThreadRead", channel_id)
        .await;
}

async fn persist_and_publish_local_reductions(
    events: &RuntimeEventSender,
    store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    action: &'static str,
    channel_id: &str,
) {
    match workspace.recover_persisted_admitted(store).await {
        Ok(publication) => {
            for write in publication.writes() {
                publish_persisted_workspace_write(events, write);
            }
        }
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "{action}WorkspaceBatchDeferred channel_id={channel_id} category={:?}",
                    error.category()
                ),
            );
        }
    }
}

/// Keeps storeless Socket Mode sessions on the same ordered reducer lane.
///
/// Without a cache store there is no durable claim ledger. Notifications from
/// this path are therefore explicitly session-only and unclaimed, while the
/// raw timeline event still precedes every typed patch derived from the current message.
async fn publish_socket_message_without_store(
    current_user_id: Option<&str>,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    message_event: crate::socket_mode::SocketModeMessageEvent,
) {
    let _admission = workspace.publication_admission.lock().await;

    let recovered = match workspace.recover_persisted_admitted(None).await {
        Ok(writes) => writes,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "StorelessRealtimeWorkspaceRecoveryDeferred category={:?}",
                    error.category()
                ),
            );
            events.send_event(RuntimeEventKind::SocketModeEvent {
                event: SocketModeEvent::Message(Box::new(message_event)),
                attention: None,
            });
            return;
        }
    };
    for write in recovered.writes() {
        publish_persisted_workspace_write(events, write);
    }
    drop(recovered);

    let classified = classify_socket_attention(workspace, current_user_id, &message_event);
    let delivery = match classified {
        AttentionPersistenceStatus::Accepted | AttentionPersistenceStatus::NotApplicable => None,
        AttentionPersistenceStatus::AlreadyObserved => Some(DeliveryState::Duplicate),
        AttentionPersistenceStatus::AtOrBeforeReadCursor => Some(DeliveryState::Stale),
        AttentionPersistenceStatus::Failed => None,
    };
    let current = workspace.apply_and_enqueue(
        None,
        MutationOrigin::Realtime,
        realtime_message_mutation(&message_event, delivery),
    );
    let applied_attention = current.as_ref().and_then(|reduction| {
        reduction.effects().iter().find_map(|effect| match effect {
            WorkspaceEffect::MessageAttention(effect) => Some(effect.clone()),
            WorkspaceEffect::ThreadRead(_) => None,
        })
    });
    let attention_status =
        if current.is_none() && classified == AttentionPersistenceStatus::Accepted {
            AttentionPersistenceStatus::AlreadyObserved
        } else {
            classified
        };

    let attention = (attention_status == AttentionPersistenceStatus::Accepted)
        .then(|| {
            applied_attention
                .as_ref()
                .map(|effect| effect.decision.clone())
        })
        .flatten();
    let writes = workspace.drain_persisted_admitted();
    events.send_event(RuntimeEventKind::SocketModeEvent {
        event: SocketModeEvent::Message(Box::new(message_event)),
        attention,
    });

    // These reductions are immediately publishable in memory, but their
    // persistence-only claim intents were not durably claimed.
    for write in &writes {
        publish_persisted_workspace_write(events, write);
    }
    if attention_status == AttentionPersistenceStatus::Accepted {
        if let Some(notification) =
            applied_attention.filter(|effect| effect.decision.send_notification)
        {
            events.send_event(RuntimeEventKind::AttentionNotificationCandidate {
                channel_id: notification.channel_id,
                message: Box::new(notification.message),
                decision: notification.decision,
            });
        }
    }
}

/// Serializes socket message authority with interactive history snapshots.
///
/// The timeline UI still consumes the raw socket event. Older recovered patches
/// publish first, while the current raw event must precede its typed patch so
/// the window can retain the first-unread transition.
async fn persist_socket_message(
    store: &WorkspaceStore,
    current_user_id: Option<&str>,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    message_event: crate::socket_mode::SocketModeMessageEvent,
) {
    let _admission = workspace.publication_admission.lock().await;

    let recovered = match workspace.recover_persisted_admitted(Some(store)).await {
        Ok(writes) => writes,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeWorkspaceRecoveryDeferred category={:?}",
                    error.category()
                ),
            );
            let classified = classify_socket_attention(workspace, current_user_id, &message_event);
            let attention_status = if classified == AttentionPersistenceStatus::Accepted {
                AttentionPersistenceStatus::Failed
            } else {
                classified
            };
            workspace.record_attention_persistence(attention_status.metrics_outcome(), false);
            let delivery = match classified {
                AttentionPersistenceStatus::AlreadyObserved => Some(DeliveryState::Duplicate),
                AttentionPersistenceStatus::AtOrBeforeReadCursor => Some(DeliveryState::Stale),
                AttentionPersistenceStatus::NotApplicable
                | AttentionPersistenceStatus::Accepted
                | AttentionPersistenceStatus::Failed => None,
            };
            workspace.apply_and_enqueue(
                Some(store),
                MutationOrigin::Realtime,
                realtime_message_mutation(&message_event, delivery),
            );
            events.send_event(RuntimeEventKind::SocketModeEvent {
                event: SocketModeEvent::Message(Box::new(message_event)),
                attention: None,
            });
            return;
        }
    };
    for write in recovered.writes() {
        publish_persisted_workspace_write(events, write);
    }
    drop(recovered);

    let classified = classify_socket_attention(workspace, current_user_id, &message_event);
    let delivery = match classified {
        AttentionPersistenceStatus::Accepted | AttentionPersistenceStatus::NotApplicable => None,
        AttentionPersistenceStatus::AlreadyObserved => Some(DeliveryState::Duplicate),
        AttentionPersistenceStatus::AtOrBeforeReadCursor => Some(DeliveryState::Stale),
        AttentionPersistenceStatus::Failed => None,
    };
    let current = workspace.apply_and_enqueue(
        Some(store),
        MutationOrigin::Realtime,
        realtime_message_mutation(&message_event, delivery),
    );
    let applied_attention = current.as_ref().and_then(|reduction| {
        reduction.effects().iter().find_map(|effect| match effect {
            WorkspaceEffect::MessageAttention(effect) => Some(effect.clone()),
            WorkspaceEffect::ThreadRead(_) => None,
        })
    });
    let mut attention_status =
        if current.is_none() && classified == AttentionPersistenceStatus::Accepted {
            AttentionPersistenceStatus::AlreadyObserved
        } else {
            classified
        };
    let persisted = match workspace.recover_persisted_admitted(Some(store)).await {
        Ok(publication) => Some(publication),
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeMessageDeltaDeferred channel_id={} category={:?}",
                    message_event.channel_id,
                    error.category()
                ),
            );
            if attention_status == AttentionPersistenceStatus::Accepted {
                attention_status = AttentionPersistenceStatus::Failed;
            }
            None
        }
    };
    let notification_claimed = persisted.as_ref().is_some_and(|publication| {
        publication
            .writes()
            .iter()
            .any(|write| write.notification_claimed)
    });
    workspace
        .record_attention_persistence(attention_status.metrics_outcome(), notification_claimed);

    let attention = (attention_status == AttentionPersistenceStatus::Accepted)
        .then(|| {
            applied_attention
                .as_ref()
                .map(|effect| effect.decision.clone())
        })
        .flatten();
    events.send_event(RuntimeEventKind::SocketModeEvent {
        event: SocketModeEvent::Message(Box::new(message_event)),
        attention,
    });
    if let Some(publication) = persisted {
        for write in publication.writes() {
            publish_persisted_workspace_write(events, write);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketModeReconnectTiming {
    sleep: Duration,
    next_backoff: Duration,
}

fn socket_mode_reconnect_timing(
    current: Duration,
    disconnect: Option<SocketModeDisconnect>,
) -> SocketModeReconnectTiming {
    if matches!(disconnect, None | Some(SocketModeDisconnect::LinkDisabled)) {
        return SocketModeReconnectTiming {
            sleep: current,
            next_backoff: current
                .saturating_mul(2)
                .min(SOCKET_MODE_MAX_RECONNECT_DELAY),
        };
    }

    SocketModeReconnectTiming {
        sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
        next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
    }
}

async fn load_user_groups_best_effort_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    cached_user_names: HashMap<String, String>,
) -> Result<()> {
    let groups = api.user_groups().await?;

    let (names, members, loaded_user_names) =
        resolve_user_group_display_data(api, groups, cached_user_names).await;

    if !loaded_user_names.is_empty() {
        store_user_names(workspace_store, &loaded_user_names).await;
        events.send_event(RuntimeEventKind::UserNamesLoaded(loaded_user_names));
    }

    if !names.is_empty() {
        crate::debug::log(
            "runtime",
            &format!("UserGroupsLoaded count={}", names.len()),
        );
        events.send_event(RuntimeEventKind::UserGroupsLoaded { names, members });
    }
    Ok(())
}

async fn resolve_user_group_display_data(
    api: &SlackApi,
    groups: Vec<SlackUserGroup>,
    mut known_user_names: HashMap<String, String>,
) -> (
    HashMap<String, String>,
    HashMap<String, Vec<String>>,
    HashMap<String, String>,
) {
    let mut names = HashMap::new();
    let mut members = HashMap::new();
    let mut loaded_user_names = HashMap::new();

    for group in groups {
        if group.id.trim().is_empty() {
            continue;
        }

        names.insert(group.id.clone(), group.mention_label());
        let mut member_names = Vec::new();
        for user_id in group
            .users
            .iter()
            .filter(|user_id| !user_id.trim().is_empty())
        {
            if let Some(display_name) = known_user_names.get(user_id).cloned() {
                member_names.push(display_name);
                continue;
            }

            match api.user_display_name(user_id).await {
                Ok(display_name) => {
                    known_user_names.insert(user_id.clone(), display_name.clone());
                    loaded_user_names.insert(user_id.clone(), display_name.clone());
                    member_names.push(display_name);
                }
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!("UserGroupMemberNameLoadFailed user_id={user_id} error={error:#}"),
                    );
                    member_names.push(user_id.clone());
                }
            }
        }

        if !member_names.is_empty() {
            member_names.sort();
            member_names.dedup();
            members.insert(group.id, member_names);
        }
    }

    (names, members, loaded_user_names)
}

async fn persist_confirmed_conversation_star(
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    store: Option<&WorkspaceStore>,
    channel_id: String,
    starred: bool,
) -> std::result::Result<(), StoreError> {
    workspace
        .apply_persisted_and_publish_with_completion(
            store,
            events,
            MutationOrigin::Local,
            WorkspaceMutation::ConversationStarChanged {
                channel_id: channel_id.clone(),
                starred,
            },
            RuntimeEventKind::ConversationStarUpdated {
                channel_id,
                starred,
            },
        )
        .await?;
    Ok(())
}

/// Slack has already accepted this action, so action completion is independent
/// of local cache availability. The typed patch remains in the
/// ordered journal until its complete store batch is durable.
async fn persist_confirmed_reaction(
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    store: Option<&WorkspaceStore>,
    change: ReactionMutation,
    thread_ts: Option<String>,
) {
    if store.is_none() {
        let _publication = workspace.publication_admission.lock().await;
        let recovered = workspace
            .recover_persisted_admitted(None)
            .await
            .expect("no-store workspace recovery cannot fail");
        for write in recovered.writes() {
            publish_persisted_workspace_write(events, write);
        }
        drop(recovered);
        workspace.apply_and_enqueue(
            None,
            MutationOrigin::Local,
            WorkspaceMutation::ReactionChanged(change.clone()),
        );
        events.send_event(RuntimeEventKind::ReactionUpdated {
            channel_id: change.channel_id,
            ts: change.message_ts,
            name: change.name,
            added: change.added,
            thread_ts,
        });
        for write in workspace.drain_persisted_admitted() {
            publish_persisted_workspace_write(events, &write);
        }
        return;
    }

    let _admission = workspace.publication_admission.lock().await;
    workspace.apply_and_enqueue(
        store,
        MutationOrigin::Local,
        WorkspaceMutation::ReactionChanged(change.clone()),
    );
    events.send_event(RuntimeEventKind::ReactionUpdated {
        channel_id: change.channel_id.clone(),
        ts: change.message_ts.clone(),
        name: change.name.clone(),
        added: change.added,
        thread_ts,
    });

    match workspace.recover_persisted_admitted(store).await {
        Ok(publication) => {
            for write in publication.writes() {
                publish_persisted_workspace_write(events, write);
            }
        }
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "ConfirmedReactionDeltaDeferred channel_id={} category={:?}",
                    change.channel_id,
                    error.category()
                ),
            );
        }
    }
}

async fn load_conversations_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    conversation_star_sync: &ConversationStarSyncGate,
) -> Result<Vec<SlackConversation>> {
    let _star_sync_guard = conversation_star_sync.lock().await;
    events.send_status("Loading conversations");
    let base_revision = workspace.revision();
    let fresh = api.conversations().await?;
    let starred_ids = match api.starred_conversation_ids().await {
        Ok(starred_ids) => Some(starred_ids),
        Err(error) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "StarredConversationsLoadFailed category={:?}",
                    error.category()
                ),
            );
            None
        }
    };
    let conversations = apply_conversation_membership_snapshot(
        events,
        workspace_store.as_ref(),
        workspace,
        base_revision,
        fresh,
        starred_ids,
    )
    .await?;
    crate::debug::log(
        "runtime",
        &format!("ConversationsSynchronized count={}", conversations.len()),
    );
    Ok(conversations)
}

async fn apply_conversation_membership_snapshot(
    events: &RuntimeEventSender,
    workspace_store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    base_revision: WorkspaceRevision,
    conversations: Vec<SlackConversation>,
    starred_ids: Option<HashSet<String>>,
) -> Result<Vec<SlackConversation>> {
    let has_membership = conversations
        .iter()
        .any(|conversation| !conversation.id.trim().is_empty());
    if !has_membership && !workspace.conversations().is_empty() {
        return Err(anyhow!(
            "Slack returned an unexpectedly empty conversation membership snapshot"
        ));
    }

    let _admission = workspace.publication_admission.lock().await;
    if let Some(store) = workspace_store {
        store.validate_conversation_cache().await?;
        if store.workspace_cache_needs_repair() {
            workspace.repair_workspace_cache_admitted(store).await?;
        }
    }

    workspace
        .apply_persisted_and_publish_admitted(
            workspace_store,
            events,
            MutationOrigin::WebApi,
            WorkspaceMutation::MembershipSnapshot(SnapshotEnvelope::new(
                base_revision,
                ConversationMembershipSnapshot {
                    conversations,
                    starred_ids,
                },
            )),
            Some(RuntimeEventKind::ConversationsSynchronized),
        )
        .await?;
    Ok(workspace.conversations())
}

async fn load_conversations_best_effort_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_url: Option<&str>,
    workspace: WorkspacePipelineContext<'_>,
    cached_user_names: HashMap<String, String>,
    team_id: Option<&str>,
    huddles: &HuddleActorHandle,
) -> Result<()> {
    match load_conversations_with_api(
        events,
        api,
        workspace.store,
        workspace.reducer,
        workspace.conversation_star_sync,
    )
    .await
    {
        Ok(conversations) => {
            let browser_covered = apply_browser_unread_snapshot_best_effort(
                events,
                api,
                workspace_url,
                workspace.store,
                workspace.reducer,
                &conversations,
            )
            .await;
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::SyncCompleted,
            ));
            let current_huddle_channels = conversations
                .iter()
                .filter(|conversation| conversation.has_huddle_metadata())
                .map(|conversation| conversation.id.clone())
                .collect::<HashSet<_>>();
            let unread_refresh_candidates =
                uncovered_conversation_unread_refresh_candidates(&conversations, &browser_covered);
            refresh_conversation_unread_states_best_effort(
                events,
                api,
                workspace.store,
                workspace.reducer,
                unread_refresh_candidates,
            )
            .await;
            let refreshed_conversations = workspace.reducer.conversations();
            prefetch_channel_histories_best_effort(
                events,
                api,
                workspace,
                &refreshed_conversations,
                &current_huddle_channels,
                team_id,
                huddles,
            )
            .await;
            refresh_cached_conversation_user_names(
                events,
                api,
                workspace.store,
                &refreshed_conversations,
                &cached_user_names,
            )
            .await;
        }
        Err(error) => {
            handle_conversations_load_error(events, &error);
            return Err(error);
        }
    }
    Ok(())
}

fn uncovered_conversation_unread_refresh_candidates(
    conversations: &[SlackConversation],
    covered: &HashSet<String>,
) -> Vec<String> {
    conversation_unread_refresh_candidates(conversations)
        .into_iter()
        .filter(|channel_id| !covered.contains(channel_id))
        .collect()
}

fn browser_unread_snapshots_for_catalog(
    snapshot: SlackUnreadSnapshot,
    conversations: &[SlackConversation],
) -> (Vec<SlackConversationUnreadSnapshot>, HashSet<String>) {
    let known_ids = conversations
        .iter()
        .map(|conversation| conversation.id.as_str())
        .collect::<HashSet<_>>();
    let mut covered = HashSet::new();
    let snapshots = snapshot
        .channels
        .into_iter()
        .map(|record| (record, None))
        .chain(snapshot.ims.into_iter().map(|record| {
            let is_open = record.is_open;
            (record, Some(is_open))
        }))
        .chain(snapshot.mpims.into_iter().map(|record| (record, None)))
        .filter(|(record, _)| known_ids.contains(record.conversation_id.as_str()))
        .filter_map(|(record, is_open)| {
            if !covered.insert(record.conversation_id.clone()) {
                return None;
            }
            Some(browser_unread_record_snapshot(record, is_open))
        })
        .collect();
    (snapshots, covered)
}

fn browser_unread_record_snapshot(
    record: SlackUnreadSnapshotRecord,
    is_open: Option<bool>,
) -> SlackConversationUnreadSnapshot {
    SlackConversationUnreadSnapshot {
        channel_id: record.conversation_id,
        unread_state: SlackUnreadState::from_parts(true, record.has_unreads, 0),
        last_read: record.last_read,
        latest: record.latest,
        mention_count: Some(record.mention_count),
        is_open,
    }
}

#[derive(Default)]
struct PendingConversationRefreshBatch {
    refreshes: Vec<SnapshotEnvelope<ConversationRefresh>>,
    potential_changes: usize,
}

impl PendingConversationRefreshBatch {
    fn push(
        &mut self,
        refresh: SnapshotEnvelope<ConversationRefresh>,
    ) -> Option<Vec<SnapshotEnvelope<ConversationRefresh>>> {
        let potential_changes = refresh.data().potential_change_count();
        if potential_changes == 0 {
            return None;
        }
        let ready = (!self.refreshes.is_empty()
            && self.potential_changes + potential_changes > CONVERSATION_PATCH_BATCH_SIZE)
            .then(|| self.take());
        self.potential_changes += potential_changes;
        self.refreshes.push(refresh);
        ready
    }

    fn take(&mut self) -> Vec<SnapshotEnvelope<ConversationRefresh>> {
        self.potential_changes = 0;
        std::mem::take(&mut self.refreshes)
    }
}

async fn publish_conversation_refresh_batch(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    refreshes: Vec<SnapshotEnvelope<ConversationRefresh>>,
) {
    if refreshes.is_empty() {
        return;
    }
    if let Err(error) = workspace
        .apply_persisted_and_publish(
            workspace_store.as_ref(),
            events,
            MutationOrigin::WebApi,
            WorkspaceMutation::ConversationRefreshBatch(refreshes),
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!("ConversationRefreshStoreFailed error={error:#}"),
        );
    }
}

async fn apply_browser_unread_snapshot_best_effort(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_url: Option<&str>,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    conversations: &[SlackConversation],
) -> HashSet<String> {
    if api.browser_cookie_d().is_none() {
        return HashSet::new();
    }
    let base_revision = workspace.revision();
    let Some(workspace_url) = workspace_url else {
        crate::debug::log(
            "runtime",
            "BrowserUnreadSnapshotUnavailable category=Validation",
        );
        return HashSet::new();
    };
    let snapshot = match api.browser_unread_snapshot(workspace_url).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "BrowserUnreadSnapshotUnavailable category={:?}",
                    error.category()
                ),
            );
            return HashSet::new();
        }
    };
    let (snapshots, covered) = browser_unread_snapshots_for_catalog(snapshot, conversations);
    let submitted = snapshots.len();
    let mut pending = PendingConversationRefreshBatch::default();
    for snapshot in snapshots {
        let refresh = SnapshotEnvelope::new(
            base_revision,
            ConversationRefresh {
                metadata: None,
                unread: Some(snapshot),
            },
        );
        if let Some(ready) = pending.push(refresh) {
            publish_conversation_refresh_batch(events, workspace_store, workspace, ready).await;
        }
    }
    publish_conversation_refresh_batch(events, workspace_store, workspace, pending.take()).await;
    crate::debug::log(
        "runtime",
        &format!(
            "BrowserUnreadSnapshotApplied covered={} submitted={}",
            covered.len(),
            submitted
        ),
    );
    covered
}

async fn refresh_conversation_unread_states_best_effort(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    ranked_channel_ids: Vec<String>,
) {
    let cached_pending = if let Some(store) = workspace_store.as_ref() {
        match store.load_pending_unread_refresh().await {
            Ok(cached_pending) => cached_pending,
            Err(error) => {
                crate::debug::log(
                    "store",
                    &format!("PendingUnreadRefreshLoadFailed error={error:#}"),
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let ConversationUnreadRefreshPlan {
        batch: mut pending,
        queue,
        next_queue,
    } = conversation_unread_refresh_plan(
        cached_pending,
        ranked_channel_ids,
        CONVERSATION_ENRICHMENT_LIMIT,
    );
    if let Some(store) = workspace_store.as_ref() {
        if let Err(error) = store.store_pending_unread_refresh(&queue).await {
            crate::debug::log(
                "store",
                &format!("PendingUnreadRefreshStoreFailed error={error:#}"),
            );
        }
    }
    let mut refresh_batch = PendingConversationRefreshBatch::default();
    for pass in 0..MAX_UNREAD_REFRESH_PASSES {
        let mut failed = Vec::new();
        for channel_id in std::mem::take(&mut pending) {
            let base_revision = workspace.revision();
            match api.conversation_with_unread_state(&channel_id).await {
                Ok((mut details, unread_state)) => {
                    let unread_snapshot = SlackConversationUnreadSnapshot {
                        channel_id: channel_id.clone(),
                        unread_state,
                        last_read: details
                            .as_ref()
                            .and_then(|details| details.last_read_ts().map(str::to_string)),
                        latest: details
                            .as_ref()
                            .and_then(|details| details.latest_message_ts().map(str::to_string)),
                        mention_count: details
                            .as_ref()
                            .and_then(|details| details.extra.get("mention_count")?.as_u64()),
                        is_open: details
                            .as_ref()
                            .and_then(|details| details.extra.get("is_open")?.as_bool()),
                    };
                    if let Some(details) = details.as_mut() {
                        // Cursor metadata is committed atomically with unread state below.
                        details.extra.remove("last_read");
                        details.extra.remove("latest");
                        // The serialized conversation refresh and local toggle path
                        // exclusively own user-relative star state.
                        details.is_starred = None;
                        if details.is_mpim.unwrap_or(false) {
                            match api.conversation_members(&channel_id).await {
                                Ok(members) => {
                                    details.extra.insert(
                                        "members".to_string(),
                                        serde_json::json!(members),
                                    );
                                }
                                Err(error) => crate::debug::log(
                                    "runtime",
                                    &format!("ConversationMembersRefreshFailed channel_id={channel_id} error={error:#}"),
                                ),
                            }
                        }
                    }
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ConversationUnreadRefreshed channel_id={channel_id} known={} unread={} display_count={}",
                            unread_state.known, unread_state.has_unread, unread_state.display_count
                        ),
                    );
                    if !unread_state.known {
                        failed.push(channel_id.clone());
                    }
                    let refresh = SnapshotEnvelope::new(
                        base_revision,
                        ConversationRefresh {
                            metadata: details,
                            unread: unread_state.known.then_some(unread_snapshot),
                        },
                    );
                    if let Some(ready) = refresh_batch.push(refresh) {
                        publish_conversation_refresh_batch(
                            events,
                            workspace_store,
                            workspace,
                            ready,
                        )
                        .await;
                    }
                }
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!("ConversationUnreadRefreshFailed channel_id={channel_id} pass={} error={error:#}", pass + 1),
                    );
                    failed.push(channel_id);
                }
            }
        }
        pending = failed;
        if pending.is_empty() {
            break;
        }
        if pass + 1 < MAX_UNREAD_REFRESH_PASSES {
            tokio::time::sleep(UNREAD_REFRESH_RETRY_DELAY).await;
        }
    }
    if let Some(store) = workspace_store.as_ref() {
        if let Err(error) = store.store_pending_unread_refresh(&next_queue).await {
            crate::debug::log(
                "store",
                &format!("PendingUnreadRefreshStoreFailed error={error:#}"),
            );
        }
    }
    publish_conversation_refresh_batch(events, workspace_store, workspace, refresh_batch.take())
        .await;
}

async fn prefetch_channel_histories_best_effort(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace: WorkspacePipelineContext<'_>,
    conversations: &[SlackConversation],
    current_huddle_channels: &HashSet<String>,
    team_id: Option<&str>,
    huddles: &HuddleActorHandle,
) {
    let Some(store) = workspace.store.as_ref() else {
        return;
    };

    let channel_ids =
        channel_history_prefetch_candidates_with_huddles(conversations, current_huddle_channels);
    if channel_ids.is_empty() {
        return;
    }

    crate::debug::log(
        "runtime",
        &format!("ChannelHistoryPrefetchStart count={}", channel_ids.len()),
    );

    for channel_id in channel_ids {
        match store.load_history(&channel_id).await {
            Ok(Some(_)) => {
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ChannelHistoryPrefetchRefreshing channel_id={channel_id} reason=cached"
                    ),
                );
            }
            Ok(None) => {}
            Err(error) => {
                crate::debug::log(
                    "runtime",
                    &format!("ChannelHistoryPrefetchCacheCheckFailed channel_id={channel_id} error={error:#}"),
                );
                continue;
            }
        }

        let base_revision = workspace.reducer.revision();
        match api.history(&channel_id).await {
            Ok(page) => {
                if let Err(error) = publish_prefetched_history_snapshot(
                    events,
                    workspace.store,
                    workspace.reducer,
                    &channel_id,
                    base_revision,
                    page.messages.clone(),
                    page.has_more,
                    page.next_cursor.clone(),
                )
                .await
                {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "CachedHistoryStoreFailed channel_id={channel_id} error={error:#}"
                        ),
                    );
                }
                observe_huddle_messages(huddles, team_id, &channel_id, &page.messages);
                let unread_snapshot = SlackConversationUnreadSnapshot {
                    channel_id: channel_id.clone(),
                    unread_state: page.unread_state,
                    ..Default::default()
                };
                if page.unread_state.known {
                    publish_conversation_refresh_batch(
                        events,
                        workspace.store,
                        workspace.reducer,
                        vec![SnapshotEnvelope::new(
                            base_revision,
                            ConversationRefresh {
                                metadata: None,
                                unread: Some(unread_snapshot),
                            },
                        )],
                    )
                    .await;
                }
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ChannelHistoryPrefetched channel_id={channel_id} messages={}",
                        page.messages.len()
                    ),
                );
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("ChannelHistoryPrefetchFailed channel_id={channel_id} error={error:#}"),
            ),
        }
    }
}

async fn publish_prefetched_history_snapshot(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    base_revision: WorkspaceRevision,
    messages: Vec<SlackMessage>,
    has_more: bool,
    next_cursor: Option<String>,
) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
    let complete = !has_more
        && next_cursor
            .as_deref()
            .is_none_or(|cursor| cursor.trim().is_empty());
    workspace
        .apply_persisted_and_publish(
            workspace_store.as_ref(),
            events,
            MutationOrigin::WebApi,
            WorkspaceMutation::HistorySnapshot {
                channel_id: channel_id.to_string(),
                snapshot: SnapshotEnvelope::new(
                    base_revision,
                    crate::workspace_pipeline::MessagePage {
                        messages,
                        next_cursor,
                        complete,
                    },
                ),
            },
        )
        .await
}

async fn refresh_cached_conversation_user_names(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
    cached_user_names: &HashMap<String, String>,
) {
    let user_ids = cached_conversation_user_ids(conversations, cached_user_names);
    if user_ids.is_empty() {
        return;
    }

    let mut refreshed = HashMap::new();
    let mut refreshed_full_names = HashMap::new();
    let mut refreshed_avatar_urls = HashMap::new();
    for user_id in user_ids {
        match api.user(&user_id).await {
            Ok(user) => {
                if let Some(full_name) = user.full_name() {
                    refreshed_full_names.insert(user_id.clone(), full_name);
                }
                if let Some(avatar_url) = user.avatar_url() {
                    refreshed_avatar_urls.insert(user_id.clone(), avatar_url);
                }
                refreshed.insert(
                    user_id.clone(),
                    user.display_name().unwrap_or_else(|| user_id.clone()),
                );
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("UserNameRefreshFailed user_id={user_id} error={error:#}"),
            ),
        }
    }

    if refreshed.is_empty() {
        return;
    }

    store_user_names(workspace_store, &refreshed).await;
    events.send_event(RuntimeEventKind::UserNamesLoaded(refreshed));
    if !refreshed_full_names.is_empty() {
        store_user_full_names(workspace_store, &refreshed_full_names).await;
        events.send_event(RuntimeEventKind::UserFullNamesLoaded(refreshed_full_names));
    }
    if !refreshed_avatar_urls.is_empty() {
        if let Some(store) = workspace_store.as_ref() {
            if let Err(error) = store.store_user_avatar_urls(&refreshed_avatar_urls).await {
                crate::debug::log(
                    "store",
                    &format!("UserAvatarUrlsStoreFailed error={error:#}"),
                );
            }
        }
        events.send_event(RuntimeEventKind::UserAvatarUrlsLoaded(
            refreshed_avatar_urls,
        ));
    }
}

fn handle_conversations_load_error(events: &RuntimeEventSender, error: &anyhow::Error) {
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoadFailed error={error:#}"),
    );
    let failure = RuntimeFailure::from_error(error);
    events.send_event(RuntimeEventKind::WorkspaceLifecycle(
        lifecycle_failure_event(&failure),
    ));
    events.send_event(RuntimeEventKind::ConversationsLoadFailed(failure));
}

fn lifecycle_failure_event(failure: &RuntimeFailure) -> WorkspaceLifecycleEvent {
    if failure.category == RuntimeFailureCategory::Authentication {
        WorkspaceLifecycleEvent::AuthenticationFailed
    } else {
        WorkspaceLifecycleEvent::RetryableFailure
    }
}

fn send_lifecycle_failure(events: &RuntimeEventSender, error: &anyhow::Error) {
    let failure = RuntimeFailure::from_error(error);
    send_lifecycle_failure_with(events, error, failure);
}

fn send_lifecycle_failure_with(
    events: &RuntimeEventSender,
    error: &anyhow::Error,
    failure: RuntimeFailure,
) {
    events.send_event(RuntimeEventKind::WorkspaceLifecycle(
        lifecycle_failure_event(&failure),
    ));
    events.send_failure_with(error, failure);
}

#[allow(clippy::too_many_arguments)]
async fn publish_history_snapshot_with_completion(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    origin: MutationOrigin,
    base_revision: WorkspaceRevision,
    messages: Vec<SlackMessage>,
    has_more: bool,
    next_cursor: Option<String>,
    complete: bool,
    append_older: bool,
    cached: bool,
) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
    let requested_messages = messages.clone();
    let _admission = workspace.publication_admission.lock().await;
    let publication = workspace
        .apply_persisted_and_publish_retained_admitted(
            workspace_store.as_ref(),
            events,
            origin,
            WorkspaceMutation::HistorySnapshot {
                channel_id: channel_id.to_string(),
                snapshot: SnapshotEnvelope::new(
                    base_revision,
                    crate::workspace_pipeline::MessagePage {
                        messages,
                        next_cursor: next_cursor.clone(),
                        complete,
                    },
                ),
            },
            None,
        )
        .await?;
    let coordinator = workspace
        .coordinator
        .lock()
        .expect("workspace coordinator lock poisoned");
    let canonical_with_revisions = coordinator.history_with_revisions(channel_id);
    let canonical = canonical_with_revisions
        .iter()
        .map(|(message, _)| message.clone())
        .collect::<Vec<_>>();
    #[cfg(test)]
    workspace.wait_before_history_completion();
    let messages = if append_older {
        canonical_history_page_projection(&canonical, &requested_messages)
    } else if cached {
        recent_history_preview(canonical)
    } else {
        canonical_history_refresh_projection(
            &canonical_with_revisions,
            &requested_messages,
            complete,
            base_revision,
        )
    };
    events.send_event(RuntimeEventKind::HistoryLoaded {
        channel_id: channel_id.to_string(),
        messages,
        has_more,
        next_cursor,
        append_older,
        cached,
    });
    drop(coordinator);
    Ok(publication.into_reductions())
}

fn canonical_history_page_projection(
    canonical: &[SlackMessage],
    requested: &[SlackMessage],
) -> Vec<SlackMessage> {
    let mut seen = HashSet::new();
    requested
        .iter()
        .filter(|message| message.belongs_in_channel_timeline())
        .filter_map(|requested| {
            canonical
                .iter()
                .find(|candidate| same_message_identity(candidate, requested))
        })
        .filter(|message| message.belongs_in_channel_timeline())
        .filter(|message| seen.insert(message.ts.clone()))
        .cloned()
        .collect()
}

fn canonical_history_refresh_projection(
    canonical: &[(SlackMessage, WorkspaceRevision)],
    requested: &[SlackMessage],
    complete: bool,
    base_revision: WorkspaceRevision,
) -> Vec<SlackMessage> {
    let cutoff = requested
        .iter()
        .filter(|message| message.belongs_in_channel_timeline())
        .map(|message| message.ts.as_str())
        .filter(|ts| !ts.trim().is_empty())
        .min();
    let mut projected = canonical
        .iter()
        .filter(|(candidate, revision)| {
            complete
                || *revision > base_revision
                || cutoff.is_none()
                || cutoff.is_some_and(|cutoff| candidate.ts.as_str() >= cutoff)
                || requested
                    .iter()
                    .any(|requested| same_message_identity(candidate, requested))
        })
        .map(|(message, _)| message)
        .cloned()
        .collect::<Vec<_>>();
    projected.sort_by(|left, right| right.ts.cmp(&left.ts));
    projected.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    projected
}

#[allow(clippy::too_many_arguments)]
async fn publish_thread_snapshot_with_completion(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    thread_ts: &str,
    origin: MutationOrigin,
    base_revision: WorkspaceRevision,
    page: SlackMessagePage,
    append_older: bool,
) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
    let snapshot_page = crate::workspace_pipeline::MessagePage {
        messages: page.messages.clone(),
        next_cursor: page.next_cursor.clone(),
        complete: origin != MutationOrigin::Cache
            && !append_older
            && thread_page_is_complete(page.has_more, page.next_cursor.as_deref()),
    };
    publish_thread_snapshot_page_with_completion(
        events,
        workspace_store,
        workspace,
        channel_id,
        thread_ts,
        origin,
        base_revision,
        page,
        snapshot_page,
        append_older,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn publish_thread_snapshot_page_with_completion(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    thread_ts: &str,
    origin: MutationOrigin,
    base_revision: WorkspaceRevision,
    page: SlackMessagePage,
    snapshot_page: crate::workspace_pipeline::MessagePage,
    append_older: bool,
) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
    let requested_messages = page.messages.clone();
    let complete = snapshot_page.complete;
    let _admission = workspace.publication_admission.lock().await;
    let publication = workspace
        .apply_persisted_and_publish_retained_admitted(
            workspace_store.as_ref(),
            events,
            origin,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: channel_id.to_string(),
                thread_ts: thread_ts.to_string(),
                snapshot: SnapshotEnvelope::new(base_revision, snapshot_page),
            },
            None,
        )
        .await?;
    let coordinator = workspace
        .coordinator
        .lock()
        .expect("workspace coordinator lock poisoned");
    let canonical_with_revisions = coordinator.thread_with_revisions(channel_id, thread_ts);
    let canonical = canonical_with_revisions
        .iter()
        .map(|(message, _)| message.clone())
        .collect::<Vec<_>>();
    let messages = if append_older {
        canonical_thread_page_projection(&canonical, &requested_messages, thread_ts)
    } else {
        canonical_thread_refresh_projection(
            &canonical_with_revisions,
            &requested_messages,
            complete,
            base_revision,
            thread_ts,
        )
    };
    events.send_event(RuntimeEventKind::ThreadLoaded {
        channel_id: channel_id.to_string(),
        ts: thread_ts.to_string(),
        messages,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        append_older,
    });
    drop(coordinator);
    Ok(publication.into_reductions())
}

fn canonical_thread_page_projection(
    canonical: &[SlackMessage],
    requested: &[SlackMessage],
    thread_ts: &str,
) -> Vec<SlackMessage> {
    let mut seen = HashSet::new();
    requested
        .iter()
        .filter(|message| message.belongs_to_thread(thread_ts))
        .filter_map(|requested| {
            canonical
                .iter()
                .find(|candidate| same_message_identity(candidate, requested))
        })
        .filter(|message| message.belongs_to_thread(thread_ts))
        .filter(|message| seen.insert(message.ts.clone()))
        .cloned()
        .collect()
}

fn canonical_thread_refresh_projection(
    canonical: &[(SlackMessage, WorkspaceRevision)],
    requested: &[SlackMessage],
    complete: bool,
    base_revision: WorkspaceRevision,
    thread_ts: &str,
) -> Vec<SlackMessage> {
    let cutoff = requested
        .iter()
        .filter(|message| message.thread_root_ts() == Some(thread_ts))
        .map(|message| message.ts.as_str())
        .filter(|ts| !ts.trim().is_empty())
        .min();
    let mut projected = canonical
        .iter()
        .filter(|(candidate, revision)| {
            candidate.belongs_to_thread(thread_ts)
                && (complete
                    || candidate.ts == thread_ts
                    || *revision > base_revision
                    || cutoff.is_none()
                    || cutoff.is_some_and(|cutoff| candidate.ts.as_str() >= cutoff)
                    || requested
                        .iter()
                        .any(|requested| same_message_identity(candidate, requested)))
        })
        .map(|(message, _)| message)
        .cloned()
        .collect::<Vec<_>>();
    projected.sort_by(|left, right| right.ts.cmp(&left.ts));
    projected.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    projected
}

async fn mark_conversation_read_best_effort(
    api: &SlackApi,
    events: &RuntimeEventSender,
    read_marks: &mut HashMap<String, String>,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    latest_ts: &str,
) {
    if channel_id.trim().is_empty() || latest_ts.trim().is_empty() {
        return;
    }

    if read_marks
        .get(channel_id)
        .is_some_and(|marked_ts| marked_ts.as_str() >= latest_ts)
    {
        publish_local_conversation_read(
            events,
            workspace_store.as_ref(),
            workspace,
            channel_id,
            latest_ts,
        )
        .await;
        return;
    }

    if !api.can_mark_read() {
        crate::debug::log(
            "runtime",
            &format!("MarkReadSkipped channel_id={channel_id} reason=missing_token_scope"),
        );
    } else {
        match api.mark_read(channel_id, latest_ts).await {
            Ok(()) => crate::debug::log(
                "runtime",
                &format!("MarkRead channel_id={channel_id} ts={latest_ts}"),
            ),
            Err(error) => crate::debug::log(
                "runtime",
                &format!("MarkReadFailed channel_id={channel_id} ts={latest_ts} error={error:#}"),
            ),
        }
    }

    read_marks.insert(channel_id.to_string(), latest_ts.to_string());
    publish_local_conversation_read(
        events,
        workspace_store.as_ref(),
        workspace,
        channel_id,
        latest_ts,
    )
    .await;
}

async fn publish_local_conversation_read(
    events: &RuntimeEventSender,
    store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    latest_ts: &str,
) {
    if channel_id.trim().is_empty() || latest_ts.trim().is_empty() {
        return;
    }

    let _admission = workspace.publication_admission.lock().await;
    workspace.apply_and_enqueue(
        store,
        MutationOrigin::Local,
        WorkspaceMutation::ReadAdvanced {
            channel_id: channel_id.to_string(),
            ts: latest_ts.to_string(),
            remaining_unread: 0,
        },
    );
    persist_and_publish_local_reductions(
        events,
        store,
        workspace,
        "LocalConversationRead",
        channel_id,
    )
    .await;
}

fn workspace_store_id(auth: &AuthInfo) -> String {
    let team = auth
        .team_id
        .as_deref()
        .or(auth.team.as_deref())
        .or(auth.url.as_deref())
        .unwrap_or("unknown-team");
    let user = auth.user_id.as_deref().unwrap_or("unknown-user");
    format!("{team}:{user}")
}

async fn store_user_name(
    workspace_store: &Option<WorkspaceStore>,
    user_id: &str,
    display_name: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_user_name(user_id, display_name).await {
        crate::debug::log(
            "runtime",
            &format!("CachedUserNameStoreFailed user_id={user_id} error={error:#}"),
        );
    }
}

async fn store_user_full_name(
    workspace_store: &Option<WorkspaceStore>,
    user_id: &str,
    full_name: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    if let Err(error) = store
        .store_user_full_names(&HashMap::from([(
            user_id.to_string(),
            full_name.to_string(),
        )]))
        .await
    {
        crate::debug::log(
            "store",
            &format!("UserFullNameStoreFailed user_id={user_id} error={error:#}"),
        );
    }
}

async fn store_user_avatar_url(
    workspace_store: &Option<WorkspaceStore>,
    user_id: &str,
    avatar_url: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    if let Err(error) = store
        .store_user_avatar_urls(&HashMap::from([(
            user_id.to_string(),
            avatar_url.to_string(),
        )]))
        .await
    {
        crate::debug::log(
            "store",
            &format!("UserAvatarUrlStoreFailed user_id={user_id} error={error:#}"),
        );
    }
}

async fn store_user_full_names(
    workspace_store: &Option<WorkspaceStore>,
    user_full_names: &HashMap<String, String>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    if let Err(error) = store.store_user_full_names(user_full_names).await {
        crate::debug::log(
            "store",
            &format!(
                "UserFullNamesStoreFailed count={} error={error:#}",
                user_full_names.len()
            ),
        );
    }
}

async fn store_user_names(
    workspace_store: &Option<WorkspaceStore>,
    user_names: &HashMap<String, String>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_user_names(user_names).await {
        crate::debug::log(
            "runtime",
            &format!(
                "CachedUserNamesStoreFailed count={} error={error:#}",
                user_names.len()
            ),
        );
    }
}

async fn load_cached_thread(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    thread_ts: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_thread(channel_id, thread_ts).await {
        Ok(Some(messages)) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "CachedThreadLoaded channel_id={channel_id} ts={thread_ts} messages={}",
                    messages.len()
                ),
            );
            let snapshot_page = cached_thread_snapshot_page(messages.clone());
            if let Err(error) = publish_thread_snapshot_page_with_completion(
                events,
                workspace_store,
                workspace,
                channel_id,
                thread_ts,
                MutationOrigin::Cache,
                WorkspaceRevision::INITIAL,
                SlackMessagePage {
                    messages,
                    has_more: false,
                    next_cursor: None,
                    unread_state: SlackUnreadState::default(),
                },
                snapshot_page,
                false,
            )
            .await
            {
                crate::debug::log(
                    "store",
                    &format!(
                        "CachedThreadStoreFailed channel_id={channel_id} ts={thread_ts} category={:?}",
                        error.category()
                    ),
                );
            }
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!(
                "CachedThreadLoadFailed channel_id={channel_id} ts={thread_ts} error={error:#}"
            ),
        ),
    }
}

fn cached_thread_snapshot_page(
    messages: Vec<SlackMessage>,
) -> crate::workspace_pipeline::MessagePage {
    crate::workspace_pipeline::MessagePage {
        messages,
        next_cursor: None,
        complete: false,
    }
}

fn require_slack(slack: &Option<SlackApi>) -> Result<&SlackApi> {
    slack.as_ref().context("No Slack workspace is available")
}

fn message_handoff_provider_failure(error: &SlackError) -> ProviderFailure {
    if error.is_permission_denied() {
        return ProviderFailure::PermissionDenied;
    }
    if let SlackError::Api { code, .. } = error {
        if matches!(code.as_str(), "channel_not_found" | "message_not_found") {
            return ProviderFailure::NotFound;
        }
        if matches!(code.as_str(), "method_not_supported" | "unknown_method") {
            return ProviderFailure::Unsupported;
        }
    }
    match error.category() {
        SlackErrorCategory::Authentication => ProviderFailure::Authentication,
        SlackErrorCategory::Connectivity => ProviderFailure::Connectivity,
        SlackErrorCategory::RateLimited => ProviderFailure::RateLimited,
        SlackErrorCategory::LocalIo
        | SlackErrorCategory::Validation
        | SlackErrorCategory::Unexpected => ProviderFailure::Unexpected,
    }
}

trait EventSenderExt {
    fn send_status(&self, status: &str);
    fn send_failure(&self, error: &anyhow::Error);
    fn send_event(&self, event: RuntimeEventKind);
}

#[derive(Clone, Debug)]
struct RuntimeEventSender {
    sender: mpsc::UnboundedSender<RuntimeEvent>,
    session: SessionId,
    request: Option<RequestId>,
    fallback: OperationContext,
    #[cfg(test)]
    workspace_patch_send_gate: Option<Arc<TestWorkspacePatchSendGate>>,
}

#[cfg(test)]
#[derive(Debug)]
struct TestWorkspacePatchSendGate {
    started: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
#[derive(Debug)]
struct TestWorkspaceRepairAckGate {
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[cfg(test)]
impl TestWorkspaceRepairAckGate {
    async fn wait(&self) {
        if let Some(started) = self
            .started
            .lock()
            .expect("workspace repair acknowledgment start gate lock poisoned")
            .take()
        {
            let _ = started.send(());
        }
        if let Some(release) = self.release.lock().await.take() {
            let _ = release.await;
        }
    }
}

#[cfg(test)]
impl TestWorkspacePatchSendGate {
    fn wait_before_send(&self) {
        let _ = self.started.send(());
        self.release
            .lock()
            .expect("workspace patch send gate lock poisoned")
            .recv()
            .expect("workspace patch send gate release dropped");
    }
}

fn publish_workspace_reduction(events: &RuntimeEventSender, reduction: &WorkspaceReduction) {
    events.send_workspace_patch(reduction.patch().clone());
}

impl RuntimeEventSender {
    fn new(
        sender: mpsc::UnboundedSender<RuntimeEvent>,
        identity: RuntimeIdentity,
        fallback: OperationContext,
    ) -> Self {
        Self {
            sender,
            session: identity.session,
            request: Some(identity.request),
            fallback,
            #[cfg(test)]
            workspace_patch_send_gate: None,
        }
    }

    fn unsolicited(&self, context: OperationContext) -> Self {
        Self {
            sender: self.sender.clone(),
            session: self.session,
            request: None,
            fallback: context,
            #[cfg(test)]
            workspace_patch_send_gate: self.workspace_patch_send_gate.clone(),
        }
    }

    fn with_context(&self, context: OperationContext) -> Self {
        Self {
            sender: self.sender.clone(),
            session: self.session,
            request: self.request,
            fallback: context,
            #[cfg(test)]
            workspace_patch_send_gate: self.workspace_patch_send_gate.clone(),
        }
    }

    fn send_workspace_patch(&self, patch: WorkspacePatch) {
        #[cfg(test)]
        if let Some(gate) = self.workspace_patch_send_gate.as_ref() {
            gate.wait_before_send();
        }
        let _ = self.sender.send(RuntimeEvent {
            meta: RuntimeEventMeta {
                session: self.session,
                request: None,
                context: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
            },
            kind: RuntimeEventKind::WorkspacePatch(patch),
        });
    }

    fn send_failure_with(&self, error: &anyhow::Error, failure: RuntimeFailure) {
        crate::debug::log(
            "runtime",
            &format!(
                "RuntimeOperationFailed operation={:?} target={:?} category={:?} error={error:#}",
                self.fallback.operation, self.fallback.target, failure.category
            ),
        );
        self.send_event(RuntimeEventKind::Error(failure));
    }
}

impl EventSenderExt for RuntimeEventSender {
    fn send_status(&self, status: &str) {
        self.send_event(RuntimeEventKind::Status(status.to_string()));
    }

    fn send_failure(&self, error: &anyhow::Error) {
        self.send_failure_with(error, RuntimeFailure::from_error(error));
    }

    fn send_event(&self, kind: RuntimeEventKind) {
        let context = kind.operation_context(&self.fallback);
        let _ = self.sender.send(RuntimeEvent {
            meta: RuntimeEventMeta {
                session: self.session,
                request: self.request,
                context,
            },
            kind,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::sync_scheduler::{
        FreshnessPolicy, RefreshClass, ReplacementClass, RetryPolicy, SyncDurability, SyncPriority,
        SyncTargetKind,
    };
    use crate::workspace_pipeline::{ReactionMutation, StoreChange, WorkspaceChange};
    use crate::workspace_state::RealtimeMessageKind;

    #[test]
    fn stateful_catalog_compatibility_events_are_removed() {
        let production = include_str!("runtime.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        for legacy_event in [
            "ConversationsLoaded(",
            "ConversationsPatched {",
            "ConversationUnreadUpdated {",
            "ConversationMarkedRead {",
            "ConversationAttentionAcknowledged {",
            "AttentionMessagesObserved(",
            "ThreadCatalogLoaded(",
        ] {
            assert!(
                !production.contains(legacy_event),
                "stateful compatibility event remains: {legacy_event}"
            );
        }
        for legacy_adapter in [
            "send_thread_catalog_compatibility_for_reduction",
            "publish_persisted_workspace_write_after_catalog",
        ] {
            assert!(
                !production.contains(legacy_adapter),
                "thread catalog compatibility adapter remains: {legacy_adapter}"
            );
        }
    }

    #[test]
    fn store_backed_publication_drains_only_after_exclusive_recovery() {
        let production = include_str!("runtime.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let recovery = production
            .split_once("async fn recover_persisted_admitted(")
            .unwrap()
            .1
            .split_once("fn drain_persisted_admitted(")
            .unwrap()
            .0;
        assert!(recovery.contains("PersistedWorkspacePublication"));
        assert!(recovery.contains("lock_recovery_linearization().await"));
        assert!(recovery.contains("Some(recovery_publication)"));

        let realtime_reaction = production
            .split_once("async fn persist_realtime_reaction_admitted(")
            .unwrap()
            .1
            .split_once("/// Publish Slack's authoritative response")
            .unwrap()
            .0;
        assert_eq!(
            realtime_reaction
                .matches("recover_persisted_admitted(store)")
                .count(),
            2
        );
        assert!(!realtime_reaction.contains("persist_pending_writes("));
        assert!(!realtime_reaction.contains("drain_persisted_admitted("));
        assert!(realtime_reaction.contains("drop(recovered);"));
        let reaction_recovery = realtime_reaction
            .rfind("recover_persisted_admitted(store)")
            .unwrap();
        let reaction_raw = realtime_reaction
            .rfind("events.send_event(RuntimeEventKind::SocketModeEvent")
            .unwrap();
        let reaction_patch = realtime_reaction
            .rfind("publish_persisted_workspace_write")
            .unwrap();
        assert!(reaction_recovery < reaction_raw);
        assert!(reaction_raw < reaction_patch);

        let local_reductions = production
            .split_once("async fn persist_and_publish_local_reductions(")
            .unwrap()
            .1
            .split_once("/// Keeps storeless Socket Mode sessions")
            .unwrap()
            .0;
        assert!(local_reductions.contains("recover_persisted_admitted(store)"));
        assert!(!local_reductions.contains("persist_pending_writes("));
        assert!(!local_reductions.contains("drain_persisted_admitted("));

        let socket_message = production
            .split_once("async fn persist_socket_message(")
            .unwrap()
            .1
            .split_once(
                "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nstruct SocketModeReconnectTiming",
            )
            .unwrap()
            .0;
        assert_eq!(
            socket_message
                .matches("recover_persisted_admitted(Some(store))")
                .count(),
            2
        );
        assert!(!socket_message.contains("persist_pending_writes("));
        assert!(!socket_message.contains("drain_persisted_admitted("));
        assert!(socket_message.contains("drop(recovered);"));

        let confirmed_reaction = production
            .split_once("async fn persist_confirmed_reaction(")
            .unwrap()
            .1
            .split_once("async fn load_conversations_with_api(")
            .unwrap()
            .0;
        let store_backed = confirmed_reaction
            .split_once("let _admission = workspace.publication_admission.lock().await;")
            .unwrap()
            .1;
        assert!(store_backed.contains("recover_persisted_admitted(store)"));
        assert!(!store_backed.contains("persist_pending_writes("));
        assert!(!store_backed.contains("drain_persisted_admitted("));
        let reaction_raw = store_backed
            .find("events.send_event(RuntimeEventKind::ReactionUpdated")
            .unwrap();
        let reaction_recovery = store_backed
            .find("recover_persisted_admitted(store)")
            .unwrap();
        let reaction_patch = store_backed
            .find("publish_persisted_workspace_write")
            .unwrap();
        assert!(reaction_raw < reaction_recovery);
        assert!(reaction_recovery < reaction_patch);

        let history_publication = production
            .split_once("async fn publish_history_snapshot_with_completion(")
            .unwrap()
            .1
            .split_once("fn canonical_history_page_projection(")
            .unwrap()
            .0;
        assert!(history_publication.contains("apply_persisted_and_publish_retained_admitted("));
        assert!(
            history_publication
                .find("events.send_event(RuntimeEventKind::HistoryLoaded")
                .unwrap()
                < history_publication
                    .find("publication.into_reductions()")
                    .unwrap()
        );

        let thread_publication = production
            .split_once("async fn publish_thread_snapshot_page_with_completion(")
            .unwrap()
            .1
            .split_once("fn canonical_thread_page_projection(")
            .unwrap()
            .0;
        assert!(thread_publication.contains("apply_persisted_and_publish_retained_admitted("));
        assert!(
            thread_publication
                .find("events.send_event(RuntimeEventKind::ThreadLoaded")
                .unwrap()
                < thread_publication
                    .find("publication.into_reductions()")
                    .unwrap()
        );
    }

    #[test]
    fn recovered_publication_holds_exclusive_cache_guard_through_patch_delivery() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-recovery-publication-guard-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U1");
            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply_and_enqueue(
                Some(&store),
                MutationOrigin::Local,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }),
            );
            let _admission = workspace.publication_admission.lock().await;
            let publication = workspace
                .recover_persisted_admitted(Some(&store))
                .await
                .unwrap();
            assert_eq!(publication.writes().len(), 1);

            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            for write in publication.writes() {
                publish_persisted_workspace_write(&events, write);
            }
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            let reader_store = store.clone();
            let mut blocked_read =
                tokio::spawn(async move { reader_store.load_bootstrap().await.unwrap() });
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut blocked_read)
                    .await
                    .is_err(),
                "cache reads must wait until the recovered patch publication finishes"
            );
            drop(publication);
            assert!(tokio::time::timeout(Duration::from_secs(1), blocked_read)
                .await
                .unwrap()
                .unwrap()
                .is_some());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn history_completion_retains_exclusive_cache_guard_until_delivery() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-history-completion-recovery-guard-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U1");
            let workspace = WorkspaceReducerAdapter::default();
            let (sender, _receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::History,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let (completion_started, completion_reached) = std::sync::mpsc::channel();
            let (release_completion, release) = std::sync::mpsc::channel();
            workspace.set_history_completion_send_gate(Arc::new(TestWorkspacePatchSendGate {
                started: completion_started,
                release: Mutex::new(release),
            }));

            let history_store = Some(store.clone());
            let history_workspace = workspace.clone();
            let history_events = events.clone();
            let history = tokio::spawn(async move {
                publish_history_snapshot_with_completion(
                    &history_events,
                    &history_store,
                    &history_workspace,
                    "C1",
                    MutationOrigin::WebApi,
                    WorkspaceRevision::INITIAL,
                    vec![SlackMessage {
                        ts: "1.0".into(),
                        text: Some("history".into()),
                        ..Default::default()
                    }],
                    false,
                    None,
                    true,
                    false,
                    false,
                )
                .await
            });
            completion_reached
                .recv_timeout(Duration::from_secs(1))
                .expect("history completion did not reach its delivery gate");

            let reader_store = store.clone();
            let mut blocked_read =
                tokio::spawn(async move { reader_store.load_bootstrap().await.unwrap() });
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut blocked_read)
                    .await
                    .is_err(),
                "cache reads must wait until HistoryLoaded has been delivered"
            );

            release_completion.send(()).unwrap();
            history.await.unwrap().unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(1), blocked_read)
                .await
                .unwrap()
                .unwrap()
                .is_some());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn failed_unprojected_cache_reset_blocks_publication_until_full_repair() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-unprojected-cache-reset-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U1");
            store
                .store_custom_emojis(&HashMap::from([(
                    "party".to_string(),
                    "https://example.invalid/party.png".to_string(),
                )]))
                .await
                .unwrap();
            store
                .corrupt_cached_item_payload("custom_emoji", "party")
                .await
                .unwrap();
            store
                .install_workspace_reset_failure_trigger()
                .await
                .unwrap();
            assert!(store.load_custom_emojis().await.is_err());
            assert!(store.workspace_cache_needs_repair());

            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply_and_enqueue(
                Some(&store),
                MutationOrigin::Local,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }),
            );
            let _admission = workspace.publication_admission.lock().await;
            assert!(
                workspace
                    .recover_persisted_admitted(Some(&store))
                    .await
                    .is_err(),
                "publication must wait for a successful full reset"
            );
            assert_eq!(
                workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned")
                    .len(),
                1
            );

            store.clear_workspace_reset_failure_trigger().await.unwrap();
            let publication = workspace
                .recover_persisted_admitted(Some(&store))
                .await
                .unwrap();
            assert_eq!(publication.writes().len(), 1);
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            for write in publication.writes() {
                publish_persisted_workspace_write(&events, write);
            }
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            drop(publication);
            assert!(!store.workspace_cache_needs_repair());
            assert!(store.load_custom_emojis().await.unwrap().is_empty());

            let reopened = WorkspaceStore::new(directory.clone(), "T1:U1");
            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(
                bootstrap
                    .conversations
                    .into_iter()
                    .map(|conversation| conversation.id)
                    .collect::<Vec<_>>(),
                vec!["C1".to_string()]
            );
            assert!(reopened.load_custom_emojis().await.unwrap().is_empty());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    async fn apply_test_store_changes(
        store: &WorkspaceStore,
        changes: Vec<StoreChange>,
    ) -> Result<()> {
        let batch = StoreBatch::new(WorkspaceRevision::INITIAL.successor(), changes)
            .expect("test store batch must contain a change");
        assert_eq!(
            store.execute_store_batch(batch).await?,
            StoreBatchExecution::Committed,
            "test fixture batch must be the first durable workspace revision"
        );
        Ok(())
    }

    async fn seed_test_conversations(
        store: &WorkspaceStore,
        conversations: &[SlackConversation],
    ) -> Result<()> {
        apply_test_store_changes(
            store,
            vec![StoreChange::ConversationsReplaced(conversations.to_vec())],
        )
        .await
    }

    async fn seed_test_conversation(
        store: &WorkspaceStore,
        conversation: &SlackConversation,
    ) -> Result<()> {
        apply_test_store_changes(
            store,
            vec![StoreChange::ConversationUpsert(conversation.clone())],
        )
        .await
    }

    async fn load_test_conversations(
        store: &WorkspaceStore,
    ) -> Result<Option<Vec<SlackConversation>>> {
        Ok(store
            .load_bootstrap()
            .await?
            .map(|bootstrap| bootstrap.conversations)
            .filter(|conversations| !conversations.is_empty()))
    }

    async fn seed_test_history(
        store: &WorkspaceStore,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        apply_test_store_changes(
            store,
            vec![StoreChange::HistoryReplaced {
                channel_id: channel_id.to_string(),
                messages: messages.to_vec(),
            }],
        )
        .await
    }

    async fn load_test_thread_catalog(
        store: &WorkspaceStore,
    ) -> Result<Vec<crate::thread_catalog::ThreadRecord>> {
        Ok(store
            .load_bootstrap()
            .await?
            .map(|bootstrap| bootstrap.thread_catalog)
            .unwrap_or_default())
    }

    async fn claim_test_attention_delivery(
        store: &WorkspaceStore,
        revision: WorkspaceRevision,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<bool> {
        let identity =
            crate::workspace_pipeline::AttentionDeliveryIdentity::new(channel_id, message_ts)
                .expect("test attention identity must be valid");
        let batch = StoreBatch::new(
            revision,
            vec![StoreChange::AttentionNotificationClaim {
                identity: identity.clone(),
            }],
        )
        .expect("test attention claim batch must contain a change");
        let outcome = store.execute_store_batch_with_claims(batch).await?;
        Ok(outcome
            .notification_claims
            .iter()
            .any(|claim| claim.identity == identity && claim.notification_claimed))
    }

    fn thread_test_message(ts: &str, text: &str, thread_ts: Option<&str>) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            thread_ts: thread_ts.map(ToString::to_string),
            user: Some("U_OTHER".into()),
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    fn thread_test_page(
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<&str>,
    ) -> SlackMessagePage {
        SlackMessagePage {
            messages,
            has_more,
            next_cursor: next_cursor.map(ToString::to_string),
            unread_state: SlackUnreadState::default(),
        }
    }

    #[test]
    fn cached_pruned_thread_snapshot_is_always_partial() {
        let page = cached_thread_snapshot_page(vec![SlackMessage {
            ts: "10.0".into(),
            reply_count: Some(3),
            ..Default::default()
        }]);

        assert!(!page.complete);
        assert_eq!(page.messages.len(), 1);
    }

    #[test]
    fn final_older_thread_page_reconciles_the_assembled_canonical_timeline() {
        let root = SlackMessage {
            ts: "10.0".into(),
            reply_count: Some(3),
            ..Default::default()
        };
        let newer = SlackMessage {
            ts: "12.0".into(),
            thread_ts: Some("10.0".into()),
            text: Some("initial Web API value".into()),
            ..Default::default()
        };
        let deleted_during_fetch = SlackMessage {
            ts: "12.5".into(),
            thread_ts: Some("10.0".into()),
            ..Default::default()
        };
        let stale_cached = SlackMessage {
            ts: "11.5".into(),
            thread_ts: Some("10.0".into()),
            text: Some("deleted while offline".into()),
            ..Default::default()
        };
        let realtime = SlackMessage {
            ts: "13.0".into(),
            thread_ts: Some("10.0".into()),
            text: Some("arrived after the fetch started".into()),
            ..Default::default()
        };
        let older = SlackMessage {
            ts: "11.0".into(),
            thread_ts: Some("10.0".into()),
            ..Default::default()
        };

        let workspace = WorkspaceReducerAdapter::default();
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    cached_thread_snapshot_page(vec![root.clone(), stale_cached]),
                ),
            },
        );
        let fetch_base = workspace.begin_thread_fetch("C1", "10.0");
        let initial_page = vec![root, newer.clone(), deleted_during_fetch.clone()];
        workspace.record_initial_thread_page("C1", "10.0", &initial_page, false);
        workspace.apply(
            MutationOrigin::WebApi,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(
                    fetch_base,
                    crate::workspace_pipeline::MessagePage {
                        messages: initial_page,
                        next_cursor: Some("next".into()),
                        complete: false,
                    },
                ),
            },
        );
        let middle_request_base = workspace.revision();
        let middle_snapshot = workspace.older_thread_snapshot_page(
            "C1",
            "10.0",
            vec![SlackMessage {
                ts: "11.75".into(),
                thread_ts: Some("10.0".into()),
                ..Default::default()
            }],
            true,
            Some("last".into()),
            middle_request_base,
        );
        assert_eq!(middle_snapshot.base_revision(), fetch_base);
        let middle_page = middle_snapshot.into_data();
        assert!(!middle_page.complete);
        workspace.apply(
            MutationOrigin::WebApi,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(fetch_base, middle_page),
            },
        );
        workspace.apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: SlackMessage {
                    text: Some("realtime edit".into()),
                    ..newer
                },
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            },
        );
        workspace.apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: realtime,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            },
        );
        workspace.apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: deleted_during_fetch,
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            },
        );

        let final_request_base = workspace.revision();
        let snapshot = workspace.older_thread_snapshot_page(
            "C1",
            "10.0",
            vec![older],
            false,
            None,
            final_request_base,
        );

        assert_eq!(snapshot.base_revision(), fetch_base);
        let page = snapshot.into_data();
        assert!(page.complete);
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["10.0", "11.0", "11.75", "12.0", "13.0"]
        );
        assert_eq!(
            page.messages
                .iter()
                .find(|message| message.ts == "12.0")
                .and_then(|message| message.text.as_deref()),
            Some("realtime edit")
        );
    }

    #[test]
    fn final_older_thread_snapshot_retains_fetch_base_across_intervening_thread_read() {
        let workspace = WorkspaceReducerAdapter::default();
        let root = SlackMessage {
            ts: "10.0".into(),
            reply_count: Some(2),
            latest_reply: Some("12.0".into()),
            subscribed: Some(true),
            last_read: Some("10.0".into()),
            unread_count: Some(2),
            ..Default::default()
        };
        let newest = thread_test_message("12.0", "newest", Some("10.0"));
        let older = thread_test_message("11.0", "older", Some("10.0"));

        let fetch_base = workspace.begin_thread_fetch("C1", "10.0");
        let initial_page = vec![root, newest];
        workspace.record_initial_thread_page("C1", "10.0", &initial_page, false);
        workspace
            .apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "10.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        fetch_base,
                        crate::workspace_pipeline::MessagePage {
                            messages: initial_page,
                            next_cursor: Some("next".into()),
                            complete: false,
                        },
                    ),
                },
            )
            .expect("initial thread page must change the workspace");

        let read = workspace
            .apply(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".into(),
                    thread_ts: "10.0".into(),
                    ts: "12.0".into(),
                },
            )
            .expect("thread read must advance the catalog");
        let read_revision = read.patch().revision();
        assert!(read_revision > fetch_base);

        let request_base = workspace.revision();
        let snapshot = workspace.older_thread_snapshot_page(
            "C1",
            "10.0",
            vec![older],
            false,
            None,
            request_base,
        );
        assert_eq!(snapshot.base_revision(), fetch_base);
        assert!(snapshot.base_revision() < read_revision);
        assert!(read_revision <= request_base);
        assert!(snapshot.data().complete);

        workspace
            .apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "10.0".into(),
                    snapshot,
                },
            )
            .expect("final thread page must add the older reply");

        let coordinator = workspace
            .coordinator
            .lock()
            .expect("workspace coordinator lock poisoned");
        let record = coordinator
            .thread_catalog()
            .into_iter()
            .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0")
            .expect("thread catalog record missing");
        assert!(matches!(
            record.unread,
            crate::thread_catalog::ThreadUnreadState::Known {
                count: 0,
                last_read: Some(ref last_read),
            } if last_read == "12.0"
        ));
        let thread = coordinator.thread("C1", "10.0");
        assert_eq!(
            thread
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["10.0", "11.0", "12.0"]
        );
        let canonical_root = thread
            .iter()
            .find(|message| message.ts == "10.0")
            .expect("thread root missing");
        assert_eq!(canonical_root.last_read.as_deref(), Some("12.0"));
        assert_eq!(canonical_root.unread_count, Some(0));
    }

    #[test]
    fn final_older_thread_page_without_fetch_session_remains_partial() {
        let fallback_base_revision = WorkspaceRevision::INITIAL.successor();
        let snapshot = WorkspaceReducerAdapter::default().older_thread_snapshot_page(
            "C1",
            "10.0",
            vec![SlackMessage {
                ts: "11.0".into(),
                thread_ts: Some("10.0".into()),
                ..Default::default()
            }],
            false,
            None,
            fallback_base_revision,
        );

        assert_eq!(snapshot.base_revision(), fallback_base_revision);
        let page = snapshot.into_data();
        assert!(!page.complete);
        assert_eq!(page.messages.len(), 1);
    }

    async fn recv_workspace_patch_for_test(
        receiver: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> WorkspacePatch {
        match receiver
            .recv()
            .await
            .expect("runtime event channel closed")
            .kind
        {
            RuntimeEventKind::WorkspacePatch(patch) => patch,
            other => panic!("expected a workspace patch, got {other:?}"),
        }
    }

    fn conversation_patch_summary(
        changes: &[WorkspaceChange],
    ) -> Vec<(&'static str, String, Option<String>)> {
        let mut summary = changes
            .iter()
            .filter_map(|change| match change {
                WorkspaceChange::ConversationUpsert(conversation)
                | WorkspaceChange::ConversationMetadataUpsert(conversation) => {
                    Some(("upsert", conversation.id.clone(), conversation.name.clone()))
                }
                WorkspaceChange::ConversationRemoved { channel_id } => {
                    Some(("remove", channel_id.clone(), None))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        summary.sort();
        summary
    }

    fn conversation_store_summary(
        changes: &[StoreChange],
    ) -> Vec<(&'static str, String, Option<String>)> {
        let mut summary = changes
            .iter()
            .filter_map(|change| match change {
                StoreChange::ConversationUpsert(conversation)
                | StoreChange::ConversationMetadataUpsert(conversation)
                | StoreChange::ConversationMembershipUpsert(conversation) => {
                    Some(("upsert", conversation.id.clone(), conversation.name.clone()))
                }
                StoreChange::ConversationRemoved { channel_id } => {
                    Some(("remove", channel_id.clone(), None))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        summary.sort();
        summary
    }

    #[test]
    fn workspace_adapter_returns_authoritative_conversation_membership_commit() {
        let workspace = WorkspaceReducerAdapter::default();
        let general = SlackConversation {
            id: "C1".into(),
            name: Some("general".into()),
            is_channel: Some(true),
            ..Default::default()
        };
        let removable = SlackConversation {
            id: "C_OLD".into(),
            name: Some("old".into()),
            is_channel: Some(true),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![general.clone(), removable],
                    ..Default::default()
                }),
            )
            .expect("cache hydration should return its authoritative commit");
        let snapshot_base = workspace.revision();

        let mut renamed = general.clone();
        renamed.name = Some("announcements".into());
        workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::ConversationUpsert(renamed.clone()),
            )
            .expect("realtime metadata should return its authoritative commit");
        let local_membership = SlackConversation {
            id: "C_LOCAL".into(),
            name: Some("joined-during-request".into()),
            is_channel: Some(true),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Local,
                WorkspaceMutation::ConversationUpsert(local_membership.clone()),
            )
            .expect("local membership should return its authoritative commit");

        let discovered = SlackConversation {
            id: "C2".into(),
            name: Some("new-channel".into()),
            is_channel: Some(true),
            ..Default::default()
        };
        let reduction = workspace
            .apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::MembershipSnapshot(SnapshotEnvelope::new(
                    snapshot_base,
                    ConversationMembershipSnapshot {
                        conversations: vec![general, discovered],
                        starred_ids: None,
                    },
                )),
            )
            .expect("membership changes should return one authoritative commit");

        let expected = vec![
            ("remove", "C_OLD".to_string(), None),
            ("upsert", "C2".to_string(), Some("new-channel".to_string())),
        ];
        assert_eq!(
            conversation_patch_summary(reduction.patch().changes()),
            expected
        );
        let store_batch = reduction
            .store_batch()
            .expect("conversation membership must be persisted");
        assert_eq!(store_batch.revision(), reduction.patch().revision());
        assert_eq!(
            conversation_store_summary(store_batch.changes()),
            expected,
            "the patch and store batch must describe the same authority change"
        );

        assert!(
            workspace
                .apply(
                    MutationOrigin::Realtime,
                    WorkspaceMutation::ConversationUpsert(renamed),
                )
                .is_none(),
            "a stale snapshot must not overwrite newer realtime metadata"
        );
        assert!(
            workspace
                .apply(
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationUpsert(local_membership),
                )
                .is_none(),
            "a stale snapshot must not remove newer local membership"
        );
    }

    #[test]
    fn workspace_adapter_submits_each_conversation_store_batch_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-batch-submit-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            };
            assert!(
                workspace
                    .apply_persisted(
                        Some(&store),
                        MutationOrigin::WebApi,
                        WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    )
                    .await
                    .unwrap()
                    .len()
                    == 1
            );
            assert!(workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation),
                )
                .await
                .unwrap()
                .is_empty());

            let stored = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].id, "C1");
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_publishes_a_persisted_patch_as_a_session_scoped_event() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-patch-event-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let session = SessionId::default().next();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session,
                    request: RequestId::new(9),
                },
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel("C1".to_string()),
                ),
            );
            let reductions = workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C1".to_string(),
                        name: Some("general".to_string()),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert_eq!(reductions.len(), 1);

            let event = receiver.recv().await.unwrap();
            assert_eq!(event.meta.session, session);
            assert_eq!(event.meta.request, None);
            assert_eq!(
                event.meta.context,
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
            );
            assert!(matches!(
                event.kind,
                RuntimeEventKind::WorkspacePatch(patch)
                    if patch.revision() == WorkspaceRevision::INITIAL.successor()
                        && conversation_patch_summary(patch.changes())
                            == vec![("upsert", "C1".to_string(), Some("general".to_string()))]
            ));
            assert!(receiver.try_recv().is_err());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cached_bootstrap_publishes_one_typed_patch_and_keeps_other_projections() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-cached-bootstrap-event-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                ..Default::default()
            };
            let user_names = HashMap::from([("U1".to_string(), "Ada".to_string())]);
            store.store_user_names(&user_names).await.unwrap();
            let custom_emojis = HashMap::from([(
                "shipit".to_string(),
                "https://example.test/shipit.png".to_string(),
            )]);
            store.store_custom_emojis(&custom_emojis).await.unwrap();
            let root = SlackMessage {
                ts: "1.0".to_string(),
                reply_count: Some(1),
                latest_reply: Some("2.0".to_string()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".to_string()),
                    count: Some(1),
                    users: None,
                }]),
                ..Default::default()
            };
            let reply = SlackMessage {
                ts: "2.0".to_string(),
                thread_ts: Some("1.0".to_string()),
                ..Default::default()
            };
            let mut threads = crate::thread_catalog::ThreadCatalog::default();
            threads.observe_history("C1", &[root, reply]);
            let thread_records = threads.into_records();
            assert_eq!(thread_records.len(), 1);
            let reaction_actor_state = ReactionMutation {
                channel_id: "C1".to_string(),
                message_ts: "1.0".to_string(),
                name: "wave".to_string(),
                user_id: "U1".to_string(),
                added: true,
            };
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::ThreadCatalogReplaced(thread_records.clone()),
                    StoreChange::ReactionActorStatesReplaced(vec![reaction_actor_state.clone()]),
                ],
            )
            .await
            .unwrap();
            let mut canonical_thread_records = thread_records.clone();
            canonical_thread_records[0]
                .root
                .as_mut()
                .unwrap()
                .reactions
                .as_mut()
                .unwrap()[0]
                .users = Some(vec!["U1".to_string()]);
            assert_ne!(canonical_thread_records, thread_records);

            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Startup,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };
            let (huddles, _huddle_receiver) = huddle_actor_channel();
            let connection = RuntimeConnection {
                slack: SlackApi::new(StoredToken {
                    access_token: "test-token".to_string(),
                    token_type: None,
                    scope: None,
                    refresh_token: None,
                    expires_in: None,
                    expires_at: None,
                    team_id: None,
                    team_name: None,
                    user_id: None,
                    client_id: None,
                    browser_cookie_d: None,
                    user_agent: None,
                }),
                workspace_url: None,
                workspace_store: Some(store.clone()),
                workspace: workspace.clone(),
                current_user_id: None,
                user_cache: Arc::new(Mutex::new(HashMap::new())),
                read_marks: Arc::new(Mutex::new(HashMap::new())),
                message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                conversation_star_sync: ConversationStarSyncGate::default(),
                user_status_sync: UserStatusSync::default(),
                team_id: None,
                huddles,
                cached_bootstrap_load_gate: None,
            };

            load_cached_bootstrap(&events, &connection).await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                delivered
                    .iter()
                    .map(|event| match &event.kind {
                        RuntimeEventKind::UserNamesLoaded(_) => "users",
                        RuntimeEventKind::WorkspacePatch(_) => "patch",
                        RuntimeEventKind::EmojiCatalogLoaded(_) => "emoji",
                        other => panic!("unexpected cache projection event {other:?}"),
                    })
                    .collect::<Vec<_>>(),
                vec!["users", "patch", "emoji"]
            );
            let patches = delivered
                .iter()
                .filter_map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => Some(patch),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(patches.len(), 1);
            assert_eq!(patches[0].revision().value(), 1);
            assert!(matches!(
                patches[0].changes(),
                [WorkspaceChange::BootstrapReset(data)]
                    if data.conversations == vec![conversation.clone()]
                        && data.threads == canonical_thread_records
                        && data.reaction_actor_states == vec![reaction_actor_state.clone()]
            ));
            assert!(delivered.iter().any(|event| {
                matches!(
                    &event.kind,
                    RuntimeEventKind::UserNamesLoaded(names) if names == &user_names
                )
            }));
            assert!(delivered.iter().any(|event| {
                matches!(
                    &event.kind,
                    RuntimeEventKind::EmojiCatalogLoaded(emojis) if emojis == &custom_emojis
                )
            }));
            assert_eq!(
                workspace
                    .conversations()
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["C1"]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cached_bootstrap_admission_preserves_opened_conversations_for_empty_and_nonempty_cache() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            for cache_has_conversation in [false, true] {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let directory = std::env::temp_dir().join(format!(
                    "conduit-workspace-cache-open-race-{}-{nonce}-{cache_has_conversation}",
                    std::process::id()
                ));
                let store = WorkspaceStore::new(directory.clone(), "T1:U1");
                let cached = SlackConversation {
                    id: "C_CACHE".to_string(),
                    name: Some("cached".to_string()),
                    is_channel: Some(true),
                    ..Default::default()
                };
                if cache_has_conversation {
                    seed_test_conversations(&store, std::slice::from_ref(&cached))
                        .await
                        .unwrap();
                }
                store
                    .store_user_names(&HashMap::from([("U1".to_string(), "Ada".to_string())]))
                    .await
                    .unwrap();

                let workspace = WorkspaceReducerAdapter::default();
                let (sender, mut receiver) = mpsc::unbounded_channel();
                let events = RuntimeEventSender {
                    sender,
                    session: SessionId::default().next(),
                    request: None,
                    fallback: OperationContext::new(
                        RuntimeOperation::Startup,
                        RuntimeTarget::Workspace,
                    ),
                    workspace_patch_send_gate: None,
                };
                let (load_started, load_reached) = std::sync::mpsc::channel();
                let (release_load, load_release) = std::sync::mpsc::channel();
                let (huddles, _huddle_receiver) = huddle_actor_channel();
                let connection = RuntimeConnection {
                    slack: SlackApi::new(StoredToken {
                        access_token: "test-token".to_string(),
                        token_type: None,
                        scope: None,
                        refresh_token: None,
                        expires_in: None,
                        expires_at: None,
                        team_id: None,
                        team_name: None,
                        user_id: None,
                        client_id: None,
                        browser_cookie_d: None,
                        user_agent: None,
                    }),
                    workspace_url: None,
                    workspace_store: Some(store.clone()),
                    workspace: workspace.clone(),
                    current_user_id: None,
                    user_cache: Arc::new(Mutex::new(HashMap::new())),
                    read_marks: Arc::new(Mutex::new(HashMap::new())),
                    message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                    conversation_star_sync: ConversationStarSyncGate::default(),
                    user_status_sync: UserStatusSync::default(),
                    team_id: None,
                    huddles,
                    cached_bootstrap_load_gate: Some(Arc::new(TestWorkspacePatchSendGate {
                        started: load_started,
                        release: Mutex::new(load_release),
                    })),
                };
                let load_events = events.clone();
                let loading = tokio::spawn(async move {
                    load_cached_bootstrap(&load_events, &connection).await;
                });
                load_reached
                    .recv_timeout(Duration::from_secs(5))
                    .expect("cache load did not retain admission at the test gate");

                let open_workspace = workspace.clone();
                let open_store = store.clone();
                let open_events = events.clone();
                let (open_started, open_reached) = tokio::sync::oneshot::channel();
                let opening = tokio::spawn(async move {
                    let _ = open_started.send(());
                    open_workspace
                        .apply_persisted_and_publish(
                            Some(&open_store),
                            &open_events,
                            MutationOrigin::Local,
                            WorkspaceMutation::ConversationUpsert(SlackConversation {
                                id: "C_LOCAL".to_string(),
                                name: Some("opened".to_string()),
                                is_channel: Some(true),
                                ..Default::default()
                            }),
                        )
                        .await
                });
                open_reached.await.unwrap();
                tokio::task::yield_now().await;
                assert!(!opening.is_finished());
                assert_eq!(workspace.revision(), WorkspaceRevision::INITIAL);

                release_load.send(()).unwrap();
                loading.await.unwrap();
                opening.await.unwrap().unwrap();

                let ids = workspace
                    .conversations()
                    .into_iter()
                    .map(|conversation| conversation.id)
                    .collect::<Vec<_>>();
                assert_eq!(
                    ids,
                    if cache_has_conversation {
                        vec!["C_CACHE".to_string(), "C_LOCAL".to_string()]
                    } else {
                        vec!["C_LOCAL".to_string()]
                    }
                );
                let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
                assert!(matches!(
                    delivered.first().map(|event| &event.kind),
                    Some(RuntimeEventKind::UserNamesLoaded(_))
                ));
                assert_eq!(
                    delivered
                        .iter()
                        .filter_map(|event| match &event.kind {
                            RuntimeEventKind::WorkspacePatch(patch) => {
                                Some(patch.revision().value())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    if cache_has_conversation {
                        vec![1, 2]
                    } else {
                        vec![1]
                    }
                );
                let _ = std::fs::remove_dir_all(directory);
            }
        });
    }

    #[test]
    fn workspace_adapter_withholds_failed_patch_then_publishes_ordered_recovery() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-patch-recovery-event-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let session = SessionId::default().next();
            let first_events = RuntimeEventSender::new(
                sender.clone(),
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel("C1".to_string()),
                ),
            );
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &first_events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C1".to_string(),
                        name: Some("first".to_string()),
                        ..Default::default()
                    }),
                )
                .await
                .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "a patch must not precede its failed store batch"
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let recovered_events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel("C2".to_string()),
                ),
            );
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &recovered_events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C2".to_string(),
                        name: Some("second".to_string()),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();

            let first = receiver.recv().await.unwrap();
            let second = receiver.recv().await.unwrap();
            assert_eq!(first.meta.request, None);
            assert_eq!(second.meta.request, None);
            let revisions = [&first, &second]
                .into_iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch.revision().value(),
                    other => panic!("expected a workspace patch, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(revisions, vec![1, 2]);
            assert_eq!(
                [&first, &second]
                    .into_iter()
                    .flat_map(|event| match &event.kind {
                        RuntimeEventKind::WorkspacePatch(patch) => {
                            conversation_patch_summary(patch.changes())
                        }
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                vec![
                    ("upsert", "C1".to_string(), Some("first".to_string())),
                    ("upsert", "C2".to_string(), Some("second".to_string())),
                ]
            );
            assert!(receiver.try_recv().is_err());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_refresh_publishes_exactly_one_patch_and_recovers_in_revision_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-conversation-refresh-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let session = SessionId::default().next();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![
                            SlackConversation {
                                id: "C1".into(),
                                name: Some("one".into()),
                                ..Default::default()
                            },
                            SlackConversation {
                                id: "C2".into(),
                                name: Some("two".into()),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(receiver.try_recv().is_err());

            let first_base = workspace.revision();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                        first_base,
                        ConversationRefresh {
                            metadata: Some(SlackConversation {
                                id: "C1".into(),
                                name: Some("one refreshed".into()),
                                ..Default::default()
                            }),
                            unread: Some(SlackConversationUnreadSnapshot {
                                channel_id: "C1".into(),
                                unread_state: SlackUnreadState::from_parts(true, true, 1),
                                ..Default::default()
                            }),
                        },
                    ),]),
                )
                .await
                .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "a failed composite store batch must withhold its whole patch"
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let second_base = workspace.revision();
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                        second_base,
                        ConversationRefresh {
                            metadata: Some(SlackConversation {
                                id: "C2".into(),
                                name: Some("two refreshed".into()),
                                ..Default::default()
                            }),
                            unread: Some(SlackConversationUnreadSnapshot {
                                channel_id: "C2".into(),
                                unread_state: SlackUnreadState::from_parts(true, true, 2),
                                ..Default::default()
                            }),
                        },
                    )]),
                )
                .await
                .unwrap();

            let first = receiver.recv().await.unwrap();
            let second = receiver.recv().await.unwrap();
            let patches = [&first, &second]
                .into_iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch,
                    other => panic!("expected only typed workspace patches, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                patches
                    .iter()
                    .map(|patch| patch.revision().value())
                    .collect::<Vec<_>>(),
                vec![
                    first_base.successor().value(),
                    second_base.successor().value()
                ]
            );
            assert_eq!(
                patches
                    .iter()
                    .map(|patch| patch.changes().len())
                    .collect::<Vec<_>>(),
                vec![2, 2]
            );
            assert!(receiver.try_recv().is_err());

            let stored = load_test_conversations(&store).await.unwrap().unwrap();
            let one = stored
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(one.name.as_deref(), Some("one refreshed"));
            assert_eq!(one.unread_activity_count(), 1);
            let two = stored
                .iter()
                .find(|conversation| conversation.id == "C2")
                .unwrap();
            assert_eq!(two.name.as_deref(), Some("two refreshed"));
            assert_eq!(two.unread_activity_count(), 2);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn recovered_refresh_metadata_cannot_roll_back_a_local_read() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-conversation-refresh-local-read-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let view = crate::workspace_state::WorkspaceSessionState::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            let first = SlackConversation {
                id: "C1".into(),
                name: Some("one".into()),
                is_channel: Some(true),
                unread_count: Some(4),
                extra: HashMap::from([
                    ("has_unreads".into(), serde_json::json!(true)),
                    ("last_read".into(), serde_json::json!("10.0")),
                    ("latest".into(), serde_json::json!("15.0")),
                ]),
                ..Default::default()
            };
            let second = SlackConversation {
                id: "C2".into(),
                name: Some("two".into()),
                is_channel: Some(true),
                ..Default::default()
            };
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![first, second],
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            let bootstrap = receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(bootstrap) = bootstrap.kind else {
                panic!("expected a typed bootstrap patch");
            };
            view.apply_workspace_patch(&bootstrap).unwrap();

            let refresh_base = workspace.revision();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                        refresh_base,
                        ConversationRefresh {
                            metadata: Some(SlackConversation {
                                id: "C1".into(),
                                name: Some("one refreshed".into()),
                                unread_count: Some(99),
                                extra: HashMap::from([
                                    ("last_read".into(), serde_json::json!("5.0")),
                                    ("latest".into(), serde_json::json!("99.0")),
                                    ("mention_count".into(), serde_json::json!(99)),
                                    ("is_open".into(), serde_json::json!(false)),
                                    (
                                        crate::models::LOCAL_READ_TS_KEY.into(),
                                        serde_json::json!("5.0"),
                                    ),
                                ]),
                                ..Default::default()
                            }),
                            unread: Some(SlackConversationUnreadSnapshot {
                                channel_id: "C1".into(),
                                unread_state: SlackUnreadState::from_parts(true, true, 9),
                                last_read: Some("11.0".into()),
                                latest: Some("19.0".into()),
                                mention_count: Some(3),
                                is_open: Some(false),
                            }),
                        },
                    )]),
                )
                .await
                .is_err());
            assert!(receiver.try_recv().is_err());

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let workspace_store = Some(store.clone());
            publish_local_conversation_read(
                &events,
                workspace_store.as_ref(),
                &workspace,
                "C1",
                "20.0",
            )
            .await;
            let recovered_refresh = receiver.recv().await.unwrap();
            let local_read = receiver.recv().await.unwrap();
            let patches = [&recovered_refresh, &local_read]
                .into_iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch,
                    other => panic!("expected a typed workspace patch, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert!(matches!(
                patches[0].changes(),
                [
                    WorkspaceChange::ConversationMetadataUpsert(metadata),
                    WorkspaceChange::UnreadChanged { snapshot },
                ] if metadata.name.as_deref() == Some("one refreshed")
                    && snapshot.channel_id == "C1"
            ));
            assert!(matches!(
                patches[1].changes(),
                [WorkspaceChange::ConversationUpsert(conversation)]
                    if conversation.id == "C1"
                        && conversation.last_read_ts() == Some("20.0")
                        && conversation.local_read_ts() == Some("20.0")
            ));
            for patch in patches {
                view.apply_workspace_patch(patch).unwrap();
            }
            assert!(receiver.try_recv().is_err());
            let before_recovery = load_test_conversations(&store).await.unwrap().unwrap();
            let before_recovery = before_recovery
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(before_recovery.unread_activity_count(), 0);
            assert_eq!(before_recovery.last_read_ts(), Some("20.0"));
            assert_eq!(before_recovery.local_read_ts(), Some("20.0"));

            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationStarChanged {
                        channel_id: "C2".into(),
                        starred: true,
                    },
                )
                .await
                .unwrap();

            let following = receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(following) = following.kind else {
                panic!("expected a typed workspace patch");
            };
            view.apply_workspace_patch(&following).unwrap();
            assert!(receiver.try_recv().is_err());

            {
                let conversations = view.conversations();
                let current = conversations.get("C1").unwrap();
                assert_eq!(current.name.as_deref(), Some("one refreshed"));
                assert_eq!(current.unread_activity_count(), 0);
                assert_eq!(current.last_read_ts(), Some("20.0"));
                assert_eq!(current.latest_message_ts(), Some("19.0"));
            }
            let stored = load_test_conversations(&store).await.unwrap().unwrap();
            let stored = stored
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(stored.name.as_deref(), Some("one refreshed"));
            assert_eq!(stored.unread_activity_count(), 0);
            assert_eq!(stored.last_read_ts(), Some("20.0"));
            assert_eq!(stored.local_read_ts(), Some("20.0"));
            assert_eq!(stored.latest_message_ts(), Some("19.0"));
            let coordinated = workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .cloned()
                .unwrap();
            assert_eq!(coordinated.name.as_deref(), Some("one refreshed"));
            assert_eq!(coordinated.unread_activity_count(), 0);
            assert_eq!(coordinated.last_read_ts(), Some("20.0"));
            assert_eq!(coordinated.local_read_ts(), Some("20.0"));
            assert_eq!(coordinated.latest_message_ts(), Some("19.0"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn prefetched_history_respects_api_page_completeness() {
        let workspace = WorkspaceReducerAdapter::default();
        let older_cached = SlackMessage {
            ts: "1.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("older cached".into()),
            ..Default::default()
        };
        let newest = SlackMessage {
            ts: "2.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("newest".into()),
            ..Default::default()
        };
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([(
                    "C1".into(),
                    vec![older_cached.clone(), newest.clone()],
                )]),
                ..Default::default()
            }),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, _receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );

            let partial_base = workspace.revision();
            publish_prefetched_history_snapshot(
                &events,
                &None,
                &workspace,
                "C1",
                partial_base,
                vec![newest.clone()],
                true,
                Some("next".into()),
            )
            .await
            .unwrap();
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["1.0", "2.0"],
                "a partial first page must preserve older cached messages"
            );

            let complete_base = workspace.revision();
            publish_prefetched_history_snapshot(
                &events,
                &None,
                &workspace,
                "C1",
                complete_base,
                vec![newest],
                false,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["2.0"],
                "an authoritative complete page must remove absent cached messages"
            );
        });
    }

    #[test]
    fn prefetched_history_persists_the_coordinator_reconciliation_not_the_raw_page() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-prefetch-canonical-history-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            let original_edited = SlackMessage {
                ts: "1.0".into(),
                client_msg_id: Some("edited".into()),
                user: Some("U_OTHER".into()),
                text: Some("stale edit".into()),
                ..Default::default()
            };
            let original_deleted = SlackMessage {
                ts: "2.0".into(),
                client_msg_id: Some("deleted".into()),
                user: Some("U_OTHER".into()),
                text: Some("stale delete".into()),
                ..Default::default()
            };
            let original_moved = SlackMessage {
                ts: "3.0".into(),
                client_msg_id: Some("moved".into()),
                user: Some("U_OTHER".into()),
                text: Some("stale location".into()),
                ..Default::default()
            };
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            name: Some("general".into()),
                            is_channel: Some(true),
                            unread_count: Some(0),
                            extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                            ..Default::default()
                        }],
                        histories: HashMap::from([(
                            "C1".into(),
                            vec![
                                original_edited.clone(),
                                original_deleted.clone(),
                                original_moved.clone(),
                            ],
                        )]),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let network_base = workspace.revision();

            let mutations = [
                (
                    SlackMessage {
                        text: Some("authoritative edit".into()),
                        ..original_edited.clone()
                    },
                    MessageMutationKind::Changed,
                ),
                (original_deleted.clone(), MessageMutationKind::Deleted),
                (
                    SlackMessage {
                        thread_ts: Some("9.0".into()),
                        text: Some("authoritative thread location".into()),
                        ..original_moved.clone()
                    },
                    MessageMutationKind::Changed,
                ),
            ];
            for (message, kind) in mutations {
                workspace
                    .apply_persisted_and_publish(
                        Some(&store),
                        &events,
                        MutationOrigin::Realtime,
                        WorkspaceMutation::MessageChanged {
                            channel_id: "C1".into(),
                            message,
                            kind,
                            origin: MutationOrigin::Realtime,
                        },
                    )
                    .await
                    .unwrap();
                recv_workspace_patch_for_test(&mut receiver).await;
            }

            let new_message = SlackMessage {
                ts: "4.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("new from snapshot".into()),
                ..Default::default()
            };
            let reductions = publish_prefetched_history_snapshot(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                network_base,
                vec![
                    original_edited,
                    original_deleted,
                    original_moved,
                    new_message.clone(),
                ],
                false,
                None,
            )
            .await
            .unwrap();
            assert_eq!(reductions.len(), 1);
            let patch = recv_workspace_patch_for_test(&mut receiver).await;
            assert!(matches!(
                patch.changes(),
                [
                    WorkspaceChange::TimelineChanged { changes, .. },
                    WorkspaceChange::ConversationAttentionObserved {
                        channel_id,
                        observations,
                    },
                ] if matches!(
                    changes.as_slice(),
                    [crate::workspace_pipeline::MessageChange::Upsert(message)]
                        if message.ts == new_message.ts
                ) && channel_id == "C1"
                    && observations.iter().any(|item| item.message_ts == new_message.ts)
            ));
            assert!(
                receiver.try_recv().is_err(),
                "migrated prefetch must not emit a duplicate legacy attention event"
            );

            let persisted = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(
                persisted
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["4.0", "1.0"]
            );
            assert_eq!(
                persisted
                    .iter()
                    .find(|message| message.ts == "1.0")
                    .and_then(|message| message.text.as_deref()),
                Some("authoritative edit")
            );
            assert!(!persisted.iter().any(|message| message.ts == "2.0"));
            assert!(!persisted.iter().any(|message| message.ts == "3.0"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cached_history_failure_withholds_completion_and_fresh_history_recovers_fifo() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-history-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::History,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let conversation = SlackConversation {
                id: "C1".into(),
                is_channel: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                ..Default::default()
            };
            let cached = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("cached".into()),
                ..Default::default()
            };
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![cached.clone()],
                    },
                ],
            )
            .await
            .unwrap();
            let bootstrap = workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation],
                        ..Default::default()
                    }),
                )
                .unwrap();
            publish_workspace_reduction(&events, &bootstrap);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            assert!(publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::Cache,
                WorkspaceRevision::INITIAL,
                vec![cached.clone()],
                false,
                None,
                true,
                false,
                true,
            )
            .await
            .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "failed cache attention durability must withhold patch and completion"
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let fresh_base = workspace.revision();
            let fresh = SlackMessage {
                ts: "2.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("fresh".into()),
                ..Default::default()
            };
            let reductions = publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::WebApi,
                fresh_base,
                vec![fresh.clone(), cached.clone()],
                true,
                Some("older".into()),
                false,
                false,
                false,
            )
            .await
            .unwrap();

            assert_eq!(reductions.len(), 2);
            assert!(matches!(
                reductions[0].store_batch().unwrap().changes(),
                [StoreChange::ConversationAttentionObserved { .. }]
            ));
            assert!(matches!(
                reductions[1].store_batch().unwrap().changes(),
                [
                    StoreChange::HistoryReplaced { .. },
                    StoreChange::ConversationAttentionObserved { .. },
                ]
            ));
            let recovered_patch = receiver.recv().await.unwrap();
            let fresh_patch = receiver.recv().await.unwrap();
            let completion = receiver.recv().await.unwrap();
            assert!(matches!(
                recovered_patch.kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(matches!(
                fresh_patch.kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let RuntimeEventKind::HistoryLoaded {
                channel_id,
                messages,
                has_more,
                next_cursor,
                append_older,
                cached: completion_cached,
            } = completion.kind
            else {
                panic!("fresh completion must follow both FIFO patches");
            };
            assert_eq!(channel_id, "C1");
            assert_eq!(messages, vec![fresh.clone(), cached.clone()]);
            assert!(has_more);
            assert_eq!(next_cursor.as_deref(), Some("older"));
            assert!(!append_older);
            assert!(!completion_cached);
            assert!(receiver.try_recv().is_err());

            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("2.0", "fresh".into()), ("1.0", "cached".into())]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn older_history_persists_atomically_and_completes_with_page_metadata() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-older-history-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::OlderHistory,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let initial = SlackMessage {
                ts: "3.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("initial".into()),
                ..Default::default()
            };
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            is_channel: Some(true),
                            extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                            ..Default::default()
                        }],
                        histories: HashMap::from([("C1".into(), vec![initial])]),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            let newer = SlackMessage {
                ts: "2.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("newer page item".into()),
                ..Default::default()
            };
            let older = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("older page item".into()),
                ..Default::default()
            };
            let normal_reply = SlackMessage {
                ts: "1.5".into(),
                thread_ts: Some("3.0".into()),
                user: Some("U_OTHER".into()),
                text: Some("thread only".into()),
                ..Default::default()
            };
            let page = vec![newer.clone(), normal_reply, older.clone()];
            let base_revision = workspace.revision();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            assert!(publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::WebApi,
                base_revision,
                page.clone(),
                true,
                Some("page-3".into()),
                false,
                true,
                false,
            )
            .await
            .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "failed older history durability must withhold patch and completion"
            );
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0"],
                "history and attention must roll back in the same transaction"
            );
            let stored_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert!(!stored_conversation.has_observed_attention_message("2.0"));
            assert!(!stored_conversation.has_observed_attention_message("1.0"));
            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();

            let reductions = publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::WebApi,
                base_revision,
                page,
                true,
                Some("page-3".into()),
                false,
                true,
                false,
            )
            .await
            .unwrap();

            assert_eq!(reductions.len(), 1);
            assert!(matches!(
                reductions[0].store_batch().unwrap().changes(),
                [
                    StoreChange::HistoryReplaced { .. },
                    StoreChange::ThreadCatalogReplaced(_),
                    StoreChange::ConversationAttentionObserved { .. },
                ]
            ));
            recv_workspace_patch_for_test(&mut receiver).await;
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoaded {
                messages,
                has_more,
                next_cursor,
                append_older,
                cached,
                ..
            } = completion.kind
            else {
                panic!("older history must complete after its durable patch");
            };
            assert_eq!(messages, vec![newer, older]);
            assert!(has_more);
            assert_eq!(next_cursor.as_deref(), Some("page-3"));
            assert!(append_older);
            assert!(!cached);
            assert!(receiver.try_recv().is_err());
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0", "2.0", "1.0"]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn thread_snapshot_failure_withholds_publication_and_recovers_atomically() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-thread-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::Thread,
                    RuntimeTarget::Thread {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                    },
                ),
            );
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            is_channel: Some(true),
                            extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            let root = thread_test_message("10.0", "root", None);
            let reply = thread_test_message("11.0", "reply", Some("10.0"));
            let page = thread_test_page(vec![root.clone(), reply.clone()], false, None);
            let base_revision = workspace.revision();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            assert!(publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::WebApi,
                base_revision,
                page.clone(),
                false,
            )
            .await
            .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "failed thread and attention durability must withhold patch and completion"
            );
            assert!(
                store.load_thread("C1", "10.0").await.unwrap().is_none(),
                "the failed StoreBatch must roll back the thread timeline"
            );
            let stored_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert!(!stored_conversation.has_observed_attention_message("11.0"));

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let reductions = publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::WebApi,
                base_revision,
                page,
                false,
            )
            .await
            .unwrap();

            assert_eq!(reductions.len(), 1);
            assert!(matches!(
                reductions[0].store_batch().unwrap().changes(),
                [
                    StoreChange::ThreadReplaced { .. },
                    StoreChange::ThreadCatalogReplaced(_),
                    StoreChange::ConversationAttentionObserved { .. },
                ]
            ));
            assert_eq!(reductions[0].effects().len(), 2);
            recv_workspace_patch_for_test(&mut receiver).await;
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::ThreadLoaded {
                channel_id,
                ts,
                messages,
                has_more,
                next_cursor,
                append_older,
            } = completion.kind
            else {
                panic!("thread completion must follow its durable WorkspacePatch");
            };
            assert_eq!(channel_id, "C1");
            assert_eq!(ts, "10.0");
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["11.0", "10.0"]
            );
            assert!(!has_more);
            assert!(next_cursor.is_none());
            assert!(!append_older);
            assert!(
                receiver.try_recv().is_err(),
                "migrated thread snapshots must not emit duplicate legacy attention events"
            );

            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            assert_eq!(
                reopened
                    .load_thread("C1", "10.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("11.0", "reply".into()), ("10.0", "root".into())]
            );
            let reopened_conversation = load_test_conversations(&reopened)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert!(reopened_conversation.has_observed_attention_message("10.0"));
            assert!(reopened_conversation.has_observed_attention_message("11.0"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn fresh_thread_completion_preserves_concurrent_realtime_authority_everywhere() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-thread-concurrency-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::Thread,
                    RuntimeTarget::Thread {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                    },
                ),
            );
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            is_channel: Some(true),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            let root = thread_test_message("10.0", "root", None);
            let stale_edit = thread_test_message("11.0", "stale edit", Some("10.0"));
            let stale_delete = thread_test_message("12.0", "stale delete", Some("10.0"));
            let stale_move = SlackMessage {
                client_msg_id: Some("move-me".into()),
                ..thread_test_message("13.0", "stale location", Some("10.0"))
            };
            publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::WebApi,
                workspace.revision(),
                thread_test_page(
                    vec![
                        root.clone(),
                        stale_edit.clone(),
                        stale_delete.clone(),
                        stale_move.clone(),
                    ],
                    false,
                    None,
                ),
                false,
            )
            .await
            .unwrap();
            recv_workspace_patch_for_test(&mut receiver).await;
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::ThreadLoaded { .. }
            ));
            let request_base = workspace.revision();

            let authoritative_edit = SlackMessage {
                text: Some("authoritative edit".into()),
                ..stale_edit.clone()
            };
            let moved = SlackMessage {
                thread_ts: Some("20.0".into()),
                text: Some("authoritative location".into()),
                ..stale_move.clone()
            };
            let concurrent_post = thread_test_message("14.0", "concurrent post", Some("10.0"));
            for (message, kind) in [
                (authoritative_edit.clone(), MessageMutationKind::Changed),
                (stale_delete.clone(), MessageMutationKind::Deleted),
                (moved, MessageMutationKind::Changed),
                (concurrent_post.clone(), MessageMutationKind::Posted),
            ] {
                workspace
                    .apply_persisted_and_publish(
                        Some(&store),
                        &events,
                        MutationOrigin::Realtime,
                        WorkspaceMutation::MessageChanged {
                            channel_id: "C1".into(),
                            message,
                            kind,
                            origin: MutationOrigin::Realtime,
                        },
                    )
                    .await
                    .unwrap();
                recv_workspace_patch_for_test(&mut receiver).await;
            }

            let fresh = thread_test_message("15.0", "fresh reply", Some("10.0"));
            publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::WebApi,
                request_base,
                thread_test_page(
                    vec![root, stale_edit, stale_delete, stale_move, fresh.clone()],
                    false,
                    None,
                ),
                false,
            )
            .await
            .unwrap();
            recv_workspace_patch_for_test(&mut receiver).await;
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::ThreadLoaded {
                messages,
                append_older: false,
                ..
            } = completion.kind
            else {
                panic!("fresh thread must complete after its canonical patch");
            };
            let completed = messages
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>();
            assert_eq!(
                completed,
                vec![
                    ("15.0", "fresh reply".into()),
                    ("14.0", "concurrent post".into()),
                    ("11.0", "authoritative edit".into()),
                    ("10.0", "root".into()),
                ]
            );
            assert_eq!(
                store
                    .load_thread("C1", "10.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                completed
            );
            assert!(
                receiver.try_recv().is_err(),
                "thread completion must not be accompanied by a legacy attention event"
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cached_and_older_thread_snapshots_preserve_store_and_page_semantics() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-thread-pagination-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::Thread,
                    RuntimeTarget::Thread {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                    },
                ),
            );
            let conversation = SlackConversation {
                id: "C1".into(),
                is_channel: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                ..Default::default()
            };
            let root = thread_test_message("10.0", "root", None);
            let current = thread_test_message("30.0", "current reply", Some("10.0"));
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                        messages: vec![root.clone(), current.clone()],
                    },
                ],
            )
            .await
            .unwrap();
            let bootstrap = workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation],
                        ..Default::default()
                    }),
                )
                .unwrap();
            publish_workspace_reduction(&events, &bootstrap);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let cached = publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::Cache,
                WorkspaceRevision::INITIAL,
                thread_test_page(vec![root.clone(), current.clone()], false, None),
                false,
            )
            .await
            .unwrap();
            assert_eq!(cached.len(), 1);
            assert!(matches!(
                cached[0].store_batch().unwrap().changes(),
                [StoreChange::ConversationAttentionObserved { .. }]
            ));
            assert!(
                !cached[0]
                    .store_batch()
                    .unwrap()
                    .changes()
                    .iter()
                    .any(|change| matches!(change, StoreChange::ThreadReplaced { .. })),
                "cache hydration must not write its already durable thread timeline again"
            );
            recv_workspace_patch_for_test(&mut receiver).await;
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::ThreadLoaded {
                    messages,
                    append_older: false,
                    ..
                } if messages
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>() == vec!["30.0", "10.0"]
            ));

            let page_base = workspace.revision();
            let authoritative_current = SlackMessage {
                text: Some("authoritative current reply".into()),
                ..current.clone()
            };
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: authoritative_current.clone(),
                        kind: MessageMutationKind::Changed,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .unwrap();
            recv_workspace_patch_for_test(&mut receiver).await;

            let older = thread_test_message("20.0", "older reply", Some("10.0"));
            let page = thread_test_page(
                vec![root.clone(), current, older.clone()],
                true,
                Some("page-3"),
            );
            let reductions = publish_thread_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                "10.0",
                MutationOrigin::WebApi,
                page_base,
                page,
                true,
            )
            .await
            .unwrap();
            assert_eq!(reductions.len(), 1);
            assert!(matches!(
                reductions[0].store_batch().unwrap().changes(),
                [
                    StoreChange::ThreadReplaced { .. },
                    StoreChange::ThreadCatalogReplaced(_),
                    StoreChange::ConversationAttentionObserved { .. },
                ]
            ));
            recv_workspace_patch_for_test(&mut receiver).await;
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::ThreadLoaded {
                messages,
                has_more,
                next_cursor,
                append_older,
                ..
            } = completion.kind
            else {
                panic!("older thread completion must follow its durable patch");
            };
            assert_eq!(
                messages
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![
                    ("10.0", "root".into()),
                    ("30.0", "authoritative current reply".into()),
                    ("20.0", "older reply".into()),
                ]
            );
            assert!(has_more);
            assert_eq!(next_cursor.as_deref(), Some("page-3"));
            assert!(append_older);
            assert!(receiver.try_recv().is_err());

            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            assert_eq!(
                reopened
                    .load_thread("C1", "10.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![
                    ("30.0", "authoritative current reply".into()),
                    ("20.0", "older reply".into()),
                    ("10.0", "root".into()),
                ]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn duplicate_cache_fresh_and_older_history_are_idempotent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-history-idempotence-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::History,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let message = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("one".into()),
                ..Default::default()
            };
            let conversation = SlackConversation {
                id: "C1".into(),
                is_channel: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                ..Default::default()
            };
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![message.clone()],
                    },
                ],
            )
            .await
            .unwrap();
            let bootstrap = workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation],
                        ..Default::default()
                    }),
                )
                .unwrap();
            publish_workspace_reduction(&events, &bootstrap);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let workspace_store = Some(store.clone());

            let first = publish_history_snapshot_with_completion(
                &events,
                &workspace_store,
                &workspace,
                "C1",
                MutationOrigin::Cache,
                WorkspaceRevision::INITIAL,
                vec![message.clone()],
                false,
                None,
                true,
                false,
                true,
            )
            .await
            .unwrap();
            assert_eq!(first.len(), 1);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::HistoryLoaded { cached: true, .. }
            ));
            let revision = workspace.revision();

            for (origin, append_older, cached) in [
                (MutationOrigin::Cache, false, true),
                (MutationOrigin::WebApi, false, false),
                (MutationOrigin::WebApi, true, false),
            ] {
                let duplicate = publish_history_snapshot_with_completion(
                    &events,
                    &workspace_store,
                    &workspace,
                    "C1",
                    origin,
                    revision,
                    vec![message.clone()],
                    false,
                    None,
                    !append_older,
                    append_older,
                    cached,
                )
                .await
                .unwrap();
                assert!(duplicate.is_empty());
                assert_eq!(workspace.revision(), revision);
                assert!(matches!(
                    receiver.recv().await.unwrap().kind,
                    RuntimeEventKind::HistoryLoaded {
                        append_older: actual_append,
                        cached: actual_cached,
                        ..
                    } if actual_append == append_older && actual_cached == cached
                ));
                assert!(receiver.try_recv().is_err());
            }

            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["1.0"]
            );
            let stored_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert!(stored_conversation.has_observed_attention_message("1.0"));
            assert_eq!(stored_conversation.unread_activity_count(), 1);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn fresh_history_after_cache_preserves_concurrent_realtime_state_everywhere() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-interactive-history-concurrency-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::History,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let original_edit = SlackMessage {
                ts: "1.0".into(),
                client_msg_id: Some("edit".into()),
                text: Some("stale edit".into()),
                ..Default::default()
            };
            let original_deleted = SlackMessage {
                ts: "2.0".into(),
                client_msg_id: Some("deleted".into()),
                text: Some("stale delete".into()),
                ..Default::default()
            };
            let original_moved = SlackMessage {
                ts: "3.0".into(),
                client_msg_id: Some("moved".into()),
                text: Some("stale location".into()),
                ..Default::default()
            };
            let cached = vec![
                original_edit.clone(),
                original_deleted.clone(),
                original_moved.clone(),
            ];
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            is_channel: Some(true),
                            ..Default::default()
                        }],
                        histories: HashMap::from([("C1".into(), cached.clone())]),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::Cache,
                WorkspaceRevision::INITIAL,
                cached,
                false,
                None,
                true,
                false,
                true,
            )
            .await
            .unwrap();
            let cached_completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoaded {
                messages: cached_messages,
                cached: true,
                ..
            } = cached_completion.kind
            else {
                panic!("cache hydration must complete before the network base is captured");
            };
            let network_base = workspace.revision();
            let mut view = crate::workspace_state::WorkspaceViewState::default();
            view.select_conversation("C1");
            view.apply_history("C1", cached_messages, false, None, false, true);

            let authoritative_edit = SlackMessage {
                text: Some("authoritative edit".into()),
                ..original_edit.clone()
            };
            let moved_to_thread = SlackMessage {
                thread_ts: Some("9.0".into()),
                text: Some("authoritative thread location".into()),
                ..original_moved.clone()
            };
            let concurrent_post = SlackMessage {
                ts: "4.0".into(),
                text: Some("concurrent post".into()),
                ..Default::default()
            };
            let mutations = [
                (authoritative_edit.clone(), MessageMutationKind::Changed),
                (original_deleted.clone(), MessageMutationKind::Deleted),
                (moved_to_thread.clone(), MessageMutationKind::Changed),
                (concurrent_post.clone(), MessageMutationKind::Posted),
            ];
            for (message, kind) in mutations {
                workspace
                    .apply_persisted_and_publish(
                        Some(&store),
                        &events,
                        MutationOrigin::Realtime,
                        WorkspaceMutation::MessageChanged {
                            channel_id: "C1".into(),
                            message: message.clone(),
                            kind,
                            origin: MutationOrigin::Realtime,
                        },
                    )
                    .await
                    .unwrap();
                recv_workspace_patch_for_test(&mut receiver).await;
                view.apply_realtime_message(
                    "C1",
                    message,
                    match kind {
                        MessageMutationKind::Changed => RealtimeMessageKind::Changed,
                        MessageMutationKind::Deleted => RealtimeMessageKind::Deleted,
                        MessageMutationKind::Posted => RealtimeMessageKind::Posted,
                    },
                );
            }

            let fresh = SlackMessage {
                ts: "5.0".into(),
                text: Some("fresh page item".into()),
                ..Default::default()
            };
            publish_history_snapshot_with_completion(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                MutationOrigin::WebApi,
                network_base,
                vec![
                    original_edit,
                    original_deleted,
                    original_moved,
                    fresh.clone(),
                ],
                false,
                None,
                true,
                false,
                false,
            )
            .await
            .unwrap();
            recv_workspace_patch_for_test(&mut receiver).await;
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoaded {
                messages,
                append_older: false,
                cached: false,
                ..
            } = completion.kind
            else {
                panic!("fresh history must complete after its canonical patch");
            };
            assert_eq!(
                messages
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![
                    ("5.0", "fresh page item".into()),
                    ("4.0", "concurrent post".into()),
                    ("1.0", "authoritative edit".into()),
                ]
            );
            view.apply_history("C1", messages, false, None, false, false);
            assert_eq!(
                view.channel_messages("C1")
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![
                    ("5.0", "fresh page item".into()),
                    ("4.0", "concurrent post".into()),
                    ("1.0", "authoritative edit".into()),
                ]
            );
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["1.0", "4.0", "5.0"]
            );
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![
                    ("5.0", "fresh page item".into()),
                    ("4.0", "concurrent post".into()),
                    ("1.0", "authoritative edit".into()),
                ]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn recovered_history_and_message_attention_remain_read_and_publish_in_revision_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-history-attention-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let view = crate::workspace_state::WorkspaceSessionState::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            let initial_message = SlackMessage {
                ts: "10.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("initial".into()),
                ..Default::default()
            };
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            name: Some("general".into()),
                            is_channel: Some(true),
                            is_starred: Some(true),
                            unread_count: Some(2),
                            extra: HashMap::from([
                                ("has_unreads".into(), serde_json::json!(true)),
                                ("last_read".into(), serde_json::json!("10.0")),
                                ("latest".into(), serde_json::json!("15.0")),
                                ("topic".into(), serde_json::json!("Keep me")),
                            ]),
                            ..Default::default()
                        }],
                        histories: HashMap::from([("C1".into(), vec![initial_message.clone()])]),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            let bootstrap = receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(bootstrap) = bootstrap.kind else {
                panic!("expected a typed bootstrap patch");
            };
            view.apply_workspace_patch(&bootstrap).unwrap();

            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            let history_base = workspace.revision();
            let history_message = SlackMessage {
                ts: "11.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("history attention".into()),
                ..Default::default()
            };
            assert!(publish_prefetched_history_snapshot(
                &events,
                &Some(store.clone()),
                &workspace,
                "C1",
                history_base,
                vec![initial_message, history_message],
                false,
                None,
            )
            .await
            .is_err());
            assert!(receiver.try_recv().is_err());
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["10.0"],
                "the failed history and attention batch must roll back together"
            );

            let posted_message = SlackMessage {
                ts: "12.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("posted attention".into()),
                ..Default::default()
            };
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: posted_message,
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .is_err());
            assert!(receiver.try_recv().is_err());

            let after_cursor_message = SlackMessage {
                ts: "21.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("must remain unread after recovery".into()),
                ..Default::default()
            };
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: after_cursor_message,
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .is_err());
            assert!(receiver.try_recv().is_err());

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let workspace_store = Some(store.clone());
            publish_local_conversation_read(
                &events,
                workspace_store.as_ref(),
                &workspace,
                "C1",
                "20.0",
            )
            .await;

            let patches = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| match event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch,
                    other => panic!("expected only typed workspace patches, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                patches
                    .iter()
                    .map(|patch| patch.revision().value())
                    .collect::<Vec<_>>(),
                vec![
                    history_base.successor().value(),
                    history_base.successor().successor().value(),
                    history_base.successor().successor().successor().value(),
                    history_base
                        .successor()
                        .successor()
                        .successor()
                        .successor()
                        .value(),
                ]
            );
            assert_eq!(
                patches
                    .iter()
                    .take(3)
                    .map(|patch| {
                        patch
                            .changes()
                            .iter()
                            .find_map(|change| match change {
                                WorkspaceChange::ConversationAttentionObserved {
                                    observations,
                                    ..
                                } => observations
                                    .first()
                                    .map(|observation| observation.message_ts.as_str()),
                                _ => None,
                            })
                            .unwrap()
                    })
                    .collect::<Vec<_>>(),
                vec!["11.0", "12.0", "21.0"]
            );
            assert!(matches!(
                patches[3].changes(),
                [WorkspaceChange::ConversationUpsert(conversation)]
                    if conversation.id == "C1"
                        && conversation.last_read_ts() == Some("20.0")
                        && conversation.local_read_ts() == Some("20.0")
            ));
            for patch in &patches {
                view.apply_workspace_patch(patch).unwrap();
            }

            let view_current = view.conversations().get("C1").cloned().unwrap();
            let stored_current = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            let coordinator_current = workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .cloned()
                .unwrap();
            for current in [view_current, stored_current, coordinator_current] {
                assert_eq!(current.name.as_deref(), Some("general"));
                assert!(current.is_starred());
                assert_eq!(current.raw_unread_activity_count(), 0);
                assert_eq!(current.unread_activity_count(), 1);
                assert!(current.has_unread_activity());
                assert_eq!(current.last_read_ts(), Some("20.0"));
                assert_eq!(
                    current.extra.get("topic"),
                    Some(&serde_json::json!("Keep me"))
                );
            }
            assert_eq!(
                load_test_conversations(&store)
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .find(|conversation| conversation.id == "C1")
                    .unwrap()
                    .local_read_ts(),
                Some("20.0")
            );
            drop(workspace_store);
            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let reopened_current = load_test_conversations(&reopened)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(reopened_current.raw_unread_activity_count(), 0);
            assert_eq!(reopened_current.unread_activity_count(), 1);
            assert_eq!(reopened_current.last_read_ts(), Some("20.0"));
            assert_eq!(reopened_current.local_read_ts(), Some("20.0"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn local_read_marker_admission_precedes_cursorless_refresh() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-local-read-refresh-admission-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let initial = SlackConversation {
                id: "C1".into(),
                unread_count: Some(4),
                extra: HashMap::from([
                    ("has_unreads".into(), serde_json::json!(true)),
                    ("latest".into(), serde_json::json!("20.0")),
                ]),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&initial))
                .await
                .unwrap();
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![initial],
                        ..Default::default()
                    }),
                )
                .unwrap();

            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            let admission = workspace.publication_admission.lock().await;
            let workspace_store = Some(store.clone());
            let local_read = publish_local_conversation_read(
                &events,
                workspace_store.as_ref(),
                &workspace,
                "C1",
                "20.0",
            );
            tokio::pin!(local_read);
            assert!(matches!(
                futures_util::poll!(&mut local_read),
                std::task::Poll::Pending
            ));
            assert_eq!(
                workspace
                    .coordinator
                    .lock()
                    .unwrap()
                    .conversation("C1")
                    .unwrap()
                    .local_read_ts(),
                None
            );
            let before_release = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(before_release[0].local_read_ts(), None);
            assert_eq!(before_release[0].unread_activity_count(), 4);

            drop(admission);
            local_read.await;
            assert_eq!(
                workspace
                    .coordinator
                    .lock()
                    .unwrap()
                    .conversation("C1")
                    .unwrap()
                    .local_read_ts(),
                Some("20.0")
            );
            let persisted = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(persisted[0].local_read_ts(), Some("20.0"));
            assert_eq!(persisted[0].unread_activity_count(), 0);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(receiver.try_recv().is_err());
            let base_revision = workspace.revision();
            let reductions = workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                        base_revision,
                        ConversationRefresh {
                            metadata: None,
                            unread: Some(SlackConversationUnreadSnapshot {
                                channel_id: "C1".into(),
                                unread_state: SlackUnreadState::from_parts(true, true, 7),
                                latest: Some("30.0".into()),
                                ..Default::default()
                            }),
                        },
                    )]),
                )
                .await
                .unwrap();
            assert!(reductions.is_empty());
            assert_eq!(workspace.revision(), base_revision);
            assert!(receiver.try_recv().is_err());
            let persisted = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(persisted[0].local_read_ts(), Some("20.0"));
            assert_eq!(persisted[0].unread_activity_count(), 0);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_serializes_patch_and_completion_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-patch-publication-order-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let session = SessionId::default().next();
            let (publication_started, publication_reached) = std::sync::mpsc::channel();
            let (release_publication, release) = std::sync::mpsc::channel();
            let mut first_events = RuntimeEventSender::new(
                sender.clone(),
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel("C1".to_string()),
                ),
            );
            first_events.workspace_patch_send_gate = Some(Arc::new(TestWorkspacePatchSendGate {
                started: publication_started,
                release: Mutex::new(release),
            }));
            let second_events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::Channel("C2".to_string()),
                ),
            );

            let first_workspace = workspace.clone();
            let first_store = store.clone();
            let first = tokio::spawn(async move {
                first_workspace
                    .apply_persisted_and_publish_with_completion(
                        Some(&first_store),
                        &first_events,
                        MutationOrigin::WebApi,
                        WorkspaceMutation::ConversationUpsert(SlackConversation {
                            id: "C1".to_string(),
                            ..Default::default()
                        }),
                        RuntimeEventKind::ConversationOpened {
                            channel_id: "C1".to_string(),
                        },
                    )
                    .await
            });
            publication_reached
                .recv_timeout(Duration::from_secs(5))
                .expect("first patch publication did not reach the test gate");

            let (second_started, second_reached) = tokio::sync::oneshot::channel();
            let second_workspace = workspace.clone();
            let second_store = store.clone();
            let second = tokio::spawn(async move {
                let _ = second_started.send(());
                second_workspace
                    .apply_persisted_and_publish_with_completion(
                        Some(&second_store),
                        &second_events,
                        MutationOrigin::WebApi,
                        WorkspaceMutation::ConversationUpsert(SlackConversation {
                            id: "C2".to_string(),
                            ..Default::default()
                        }),
                        RuntimeEventKind::ConversationOpened {
                            channel_id: "C2".to_string(),
                        },
                    )
                    .await
            });
            second_reached.await.unwrap();
            tokio::task::yield_now().await;
            assert!(!second.is_finished());
            assert!(
                receiver.try_recv().is_err(),
                "the later patch must remain behind the first publication section"
            );

            release_publication.send(()).unwrap();
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();

            let mut sequence = Vec::new();
            for _ in 0..4 {
                sequence.push(match receiver.recv().await.unwrap().kind {
                    RuntimeEventKind::WorkspacePatch(patch) => {
                        format!("patch:{}", patch.revision().value())
                    }
                    RuntimeEventKind::ConversationOpened { channel_id } => {
                        format!("opened:{channel_id}")
                    }
                    other => panic!("unexpected ordered event {other:?}"),
                });
            }
            assert_eq!(
                sequence,
                vec!["patch:1", "opened:C1", "patch:2", "opened:C2"]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_retries_failed_batch_before_accepting_the_next_mutation() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-batch-retry-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let first = SlackConversation {
                id: "C1".into(),
                name: Some("first".into()),
                ..Default::default()
            };
            store
                .install_conversation_batch_failure_trigger()
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(first.clone()),
                )
                .await
                .is_err());
            assert!(load_test_conversations(&store).await.unwrap().is_none());
            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();

            let second = SlackConversation {
                id: "C2".into(),
                name: Some("second".into()),
                ..Default::default()
            };
            let (retry, next) = futures_util::future::join(
                workspace.apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(first),
                ),
                workspace.apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(second),
                ),
            )
            .await;
            let mut reductions = retry.unwrap();
            reductions.extend(next.unwrap());
            assert_eq!(
                reductions
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
                    .collect::<Vec<_>>(),
                vec![1, 2],
                "the recovered patch must remain ahead of the next mutation patch"
            );

            let stored = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(
                stored
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .collect::<HashSet<_>>(),
                HashSet::from(["C1", "C2"])
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_retains_an_admitted_reduction_when_persistence_is_cancelled() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-batch-cancel-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (started, writer_started) = tokio::sync::oneshot::channel();
            let (release_writer, release) = std::sync::mpsc::channel();
            let blocking_store = store.clone();
            let blocker = tokio::spawn(async move {
                blocking_store
                    .occupy_writer_until(started, release)
                    .await
                    .unwrap();
            });
            writer_started.await.unwrap();

            let first = SlackConversation {
                id: "C1".into(),
                name: Some("first".into()),
                ..Default::default()
            };
            let cancelled_workspace = workspace.clone();
            let cancelled_store = store.clone();
            let cancelled = tokio::spawn(async move {
                cancelled_workspace
                    .apply_persisted(
                        Some(&cancelled_store),
                        MutationOrigin::WebApi,
                        WorkspaceMutation::ConversationUpsert(first),
                    )
                    .await
            });
            while workspace.revision() == WorkspaceRevision::INITIAL {
                tokio::task::yield_now().await;
            }
            cancelled.abort();
            assert!(cancelled.await.unwrap_err().is_cancelled());
            release_writer.send(()).unwrap();
            blocker.await.unwrap();

            let reductions = workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C2".into(),
                        name: Some("second".into()),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert_eq!(
                reductions
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
            let stored = load_test_conversations(&store).await.unwrap().unwrap();
            assert_eq!(
                stored
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .collect::<HashSet<_>>(),
                HashSet::from(["C1", "C2"])
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_retains_recovered_patch_when_the_following_batch_fails() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-batch-consecutive-failure-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C1".into(),
                        ..Default::default()
                    }),
                )
                .await
                .is_err());

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            store
                .install_conversation_batch_failure_trigger_for("C2")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C2".into(),
                        ..Default::default()
                    }),
                )
                .await
                .is_err());

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let reductions = workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C2".into(),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert_eq!(
                reductions
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_journals_an_opposite_star_without_premature_completion() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-star-retry-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel("C1".to_string()),
            ),
            workspace_patch_send_gate: None,
        };

        runtime.block_on(async {
            let initial = SlackConversation {
                id: "C1".into(),
                is_channel: Some(true),
                is_starred: Some(false),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![initial.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, std::slice::from_ref(&initial))
                .await
                .unwrap();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationStarChanged {
                        channel_id: "C1".into(),
                        starred: true,
                    },
                )
                .await
                .unwrap_err();
            assert!(workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred());
            assert!(!load_test_conversations(&store).await.unwrap().unwrap()[0].is_starred());

            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationStarChanged {
                        channel_id: "C1".into(),
                        starred: false,
                    },
                )
                .await
                .unwrap_err();
            assert!(!workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred());
            assert!(!load_test_conversations(&store).await.unwrap().unwrap()[0].is_starred());
            assert!(
                receiver.try_recv().is_err(),
                "failed star batches must not publish completion events"
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            let reductions = workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationStarChanged {
                        channel_id: "C1".into(),
                        starred: false,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                reductions
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
                    .collect::<Vec<_>>(),
                vec![2, 3]
            );
            assert_eq!(
                reductions
                    .iter()
                    .flat_map(|reduction| reduction.patch().changes())
                    .filter_map(|change| match change {
                        WorkspaceChange::ConversationUpsert(conversation) => {
                            conversation.is_starred
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec![true, false]
            );
            let first = receiver.recv().await.unwrap();
            let second = receiver.recv().await.unwrap();
            assert_eq!(
                [&first, &second]
                    .into_iter()
                    .map(|event| match &event.kind {
                        RuntimeEventKind::WorkspacePatch(patch) => patch.revision().value(),
                        other => panic!("expected a workspace patch, got {other:?}"),
                    })
                    .collect::<Vec<_>>(),
                vec![2, 3]
            );
            assert!(receiver.try_recv().is_err());
            assert!(!load_test_conversations(&store).await.unwrap().unwrap()[0].is_starred());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_repopulates_membership_after_cache_recovery() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-membership-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let generation = store.recovery_generation();
            let base_revision = workspace.revision();
            assert!(!store.workspace_cache_needs_repair());
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: Some(RequestId::new(11)),
                fallback: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };

            assert_eq!(
                apply_conversation_membership_snapshot(
                    &events,
                    Some(&store),
                    &workspace,
                    base_revision,
                    vec![conversation.clone()],
                    None,
                )
                .await
                .unwrap(),
                vec![conversation.clone()]
            );
            assert!(store.recovery_generation() > generation);
            assert!(!store.workspace_cache_needs_repair());
            assert_eq!(workspace.revision(), base_revision);
            let completion = receiver.recv().await.unwrap();
            assert_eq!(completion.meta.request, Some(RequestId::new(11)));
            assert!(matches!(
                completion.kind,
                RuntimeEventKind::ConversationsSynchronized
            ));
            assert!(receiver.try_recv().is_err());
            assert_eq!(
                load_test_conversations(&store).await.unwrap().unwrap(),
                vec![conversation]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_repairs_the_complete_projection_after_cache_recovery() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-complete-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                unread_count: Some(2),
                extra: HashMap::from([
                    ("has_unreads".into(), serde_json::json!(true)),
                    ("last_read".into(), serde_json::json!("1.0")),
                ]),
                ..Default::default()
            };
            let user = SlackUser {
                id: Some("U1".into()),
                name: Some("ada".into()),
                real_name: Some("Ada Lovelace".into()),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: Some("Ada".into()),
                    real_name: Some("Ada Lovelace".into()),
                    status_text: Some("Working remotely".into()),
                    status_emoji: Some(":house_with_garden:".into()),
                    status_expiration: Some(0),
                    image_72: Some("https://example.test/ada.png".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "2.0".into(),
                user: Some("U1".into()),
                text: Some("root".into()),
                reply_count: Some(1),
                latest_reply: Some("3.0".into()),
                reply_users: Some(vec!["U1".into()]),
                ..Default::default()
            };
            let cached_reply = SlackMessage {
                ts: "3.0".into(),
                thread_ts: Some("2.0".into()),
                user: Some("U1".into()),
                text: Some("cached reply".into()),
                ..Default::default()
            };
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_history("C1", &[root.clone(), cached_reply.clone()]);
            let thread_records = catalog.into_records();

            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U1".into()),
            });
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation.clone()],
                        users: vec![user],
                        histories: HashMap::from([("C1".into(), vec![root.clone()])]),
                        threads: thread_records.clone(),
                        reaction_actor_states: Vec::new(),
                    }),
                )
                .unwrap();
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::ThreadSnapshot {
                        channel_id: "C1".into(),
                        thread_ts: "2.0".into(),
                        snapshot: SnapshotEnvelope::new(
                            workspace.revision(),
                            crate::workspace_pipeline::MessagePage {
                                messages: vec![root.clone(), cached_reply.clone()],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .unwrap();

            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "2.0".into(),
                        messages: vec![root.clone(), cached_reply.clone()],
                    },
                    StoreChange::ThreadCatalogReplaced(thread_records.clone()),
                ],
            )
            .await
            .unwrap();
            store
                .store_user_names(&HashMap::from([("U1".to_string(), "Ada".to_string())]))
                .await
                .unwrap();
            store
                .store_user_full_names(&HashMap::from([(
                    "U1".to_string(),
                    "Ada Lovelace".to_string(),
                )]))
                .await
                .unwrap();
            store
                .install_history_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            let fresh_message = SlackMessage {
                ts: "4.0".into(),
                user: Some("U1".into()),
                text: Some("fresh history".into()),
                ..Default::default()
            };
            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: SnapshotEnvelope::new(
                            workspace.revision(),
                            crate::workspace_pipeline::MessagePage {
                                messages: vec![root.clone(), fresh_message],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .await
                .unwrap_err();
            let pending_reply = SlackMessage {
                ts: "5.0".into(),
                thread_ts: Some("2.0".into()),
                user: Some("U1".into()),
                text: Some("pending realtime reply".into()),
                ..Default::default()
            };
            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: pending_reply.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .unwrap_err();

            let pending_revisions = {
                let pending = workspace.pending_writes.lock().unwrap();
                assert_eq!(pending.len(), 2);
                assert!(pending[0]
                    .batch
                    .as_ref()
                    .unwrap()
                    .changes()
                    .iter()
                    .any(|change| matches!(change, StoreChange::HistoryReplaced { .. })));
                assert!(pending[1]
                    .batch
                    .as_ref()
                    .unwrap()
                    .changes()
                    .iter()
                    .any(|change| matches!(change, StoreChange::MessageDelta { .. })));
                pending
                    .iter()
                    .map(|entry| entry.batch.as_ref().unwrap().revision())
                    .collect::<Vec<_>>()
            };
            let canonical_conversations = workspace.conversations();
            let canonical_thread_catalog = workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned")
                .store_projection()
                .thread_catalog;
            let mut canonical_history =
                crate::slack_message_wire::normalize_cached_messages(workspace.history("C1"));
            canonical_history.sort_by(|left, right| left.ts.cmp(&right.ts));
            let canonical_root = canonical_history
                .iter()
                .find(|message| message.ts == "2.0")
                .unwrap()
                .clone();
            let mut canonical_thread = crate::slack_message_wire::normalize_cached_messages(vec![
                canonical_root,
                cached_reply,
                pending_reply,
            ]);
            canonical_thread.sort_by(|left, right| left.ts.cmp(&right.ts));

            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();
            assert!(store.workspace_cache_needs_repair());

            {
                let _admission = workspace.publication_admission.lock().await;
                workspace
                    .repair_workspace_cache_admitted(&store)
                    .await
                    .unwrap_err();
                assert!(
                    store.workspace_cache_needs_repair(),
                    "a failed atomic repair must leave its generation pending"
                );
                assert!(workspace
                    .pending_writes
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|entry| !entry.persisted));
            }
            assert!(
                load_test_conversations(&store).await.unwrap().is_none(),
                "a failed repair transaction must not leave a partial projection"
            );
            store.clear_history_batch_failure_trigger().await.unwrap();

            let (repair_started, repair_started_receiver) = oneshot::channel();
            let (release_repair, repair_release_receiver) = oneshot::channel();
            workspace.set_workspace_repair_ack_gate(Arc::new(TestWorkspaceRepairAckGate {
                started: Mutex::new(Some(repair_started)),
                release: tokio::sync::Mutex::new(Some(repair_release_receiver)),
            }));
            let recovering_workspace = workspace.clone();
            let recovering_store = store.clone();
            let recovery = tokio::spawn(async move {
                let _admission = recovering_workspace.publication_admission.lock().await;
                recovering_workspace
                    .recover_persisted_admitted(Some(&recovering_store))
                    .await
            });
            repair_started_receiver.await.unwrap();

            let first_recovery_generation = store.recovery_generation();
            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();
            assert!(store.recovery_generation() > first_recovery_generation);
            release_repair.send(()).unwrap();

            assert_eq!(
                recovery
                    .await
                    .unwrap()
                    .unwrap()
                    .into_writes()
                    .into_iter()
                    .map(|write| write.reduction.patch().revision())
                    .collect::<Vec<_>>(),
                pending_revisions,
                "subsumed reductions must retain FIFO publication order"
            );
            assert!(!store.workspace_cache_needs_repair());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U1");
            assert_eq!(
                load_test_conversations(&reopened).await.unwrap().unwrap(),
                canonical_conversations,
                "unread-bearing conversations must match coordinator authority"
            );

            let mut reopened_history = reopened.load_history("C1").await.unwrap().unwrap();
            reopened_history.sort_by(|left, right| left.ts.cmp(&right.ts));
            assert_eq!(reopened_history, canonical_history);

            let mut reopened_thread = reopened.load_thread("C1", "2.0").await.unwrap().unwrap();
            reopened_thread.sort_by(|left, right| left.ts.cmp(&right.ts));
            assert_eq!(reopened_thread, canonical_thread);

            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(
                bootstrap.user_names.get("U1").map(String::as_str),
                Some("Ada")
            );
            assert_eq!(
                bootstrap.user_full_names.get("U1").map(String::as_str),
                Some("Ada Lovelace")
            );
            assert_eq!(
                bootstrap.user_avatar_urls.get("U1").map(String::as_str),
                Some("https://example.test/ada.png")
            );
            assert_eq!(
                bootstrap.user_search_aliases.get("U1"),
                Some(&vec!["Ada".to_string(), "Ada Lovelace".to_string(),])
            );
            assert_eq!(
                bootstrap.user_statuses.get("U1"),
                Some(&SlackUserStatus {
                    text: "Working remotely".into(),
                    emoji: ":house_with_garden:".into(),
                    expiration: 0,
                })
            );
            assert_eq!(bootstrap.thread_catalog, canonical_thread_catalog);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_repair_retries_when_coordinator_advances_before_acknowledgment() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-revision-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("before repair".into()),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();

            let (repair_started, repair_started_receiver) = oneshot::channel();
            let (release_repair, repair_release_receiver) = oneshot::channel();
            workspace.set_workspace_repair_ack_gate(Arc::new(TestWorkspaceRepairAckGate {
                started: Mutex::new(Some(repair_started)),
                release: tokio::sync::Mutex::new(Some(repair_release_receiver)),
            }));
            let recovering_workspace = workspace.clone();
            let recovering_store = store.clone();
            let recovery = tokio::spawn(async move {
                let _admission = recovering_workspace.publication_admission.lock().await;
                recovering_workspace
                    .recover_persisted_admitted(Some(&recovering_store))
                    .await
            });
            repair_started_receiver.await.unwrap();

            workspace
                .apply(
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        name: Some("advanced during repair".into()),
                        ..conversation
                    }),
                )
                .unwrap();
            release_repair.send(()).unwrap();
            assert!(recovery.await.unwrap().unwrap().is_empty());
            assert!(!store.workspace_cache_needs_repair());
            assert_eq!(
                load_test_conversations(&store).await.unwrap().unwrap()[0]
                    .name
                    .as_deref(),
                Some("advanced during repair")
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_repair_retry_replaces_only_prior_attempt_user_fields() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-user-revision-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            let first_user = SlackUser {
                id: Some("U1".into()),
                name: Some("attempt-one".into()),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: Some("Attempt One Display".into()),
                    real_name: Some("Attempt One Full".into()),
                    image_72: Some("https://example.test/attempt-one.png".into()),
                    status_text: Some("Attempt one status".into()),
                    status_emoji: Some(":one:".into()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation.clone()],
                        users: vec![first_user],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();

            let (repair_started, repair_started_receiver) = oneshot::channel();
            let (release_repair, repair_release_receiver) = oneshot::channel();
            workspace.set_workspace_repair_ack_gate(Arc::new(TestWorkspaceRepairAckGate {
                started: Mutex::new(Some(repair_started)),
                release: tokio::sync::Mutex::new(Some(repair_release_receiver)),
            }));
            let recovering_workspace = workspace.clone();
            let recovering_store = store.clone();
            let recovery = tokio::spawn(async move {
                let _admission = recovering_workspace.publication_admission.lock().await;
                recovering_workspace
                    .recover_persisted_admitted(Some(&recovering_store))
                    .await
            });
            repair_started_receiver.await.unwrap();

            store
                .store_user_name("U1", "Interleaved Compatibility Display")
                .await
                .unwrap();
            workspace
                .apply(
                    MutationOrigin::Realtime,
                    WorkspaceMutation::UserUpsert(SlackUser {
                        id: Some("U1".into()),
                        name: Some("attempt-two".into()),
                        profile: Some(crate::models::SlackUserProfile {
                            display_name: Some("Attempt Two Display".into()),
                            real_name: Some("Attempt Two Full".into()),
                            image_72: Some("https://example.test/attempt-two.png".into()),
                            status_text: Some(String::new()),
                            status_emoji: Some(String::new()),
                            status_expiration: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                )
                .unwrap();
            release_repair.send(()).unwrap();
            assert!(recovery.await.unwrap().unwrap().is_empty());

            let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(
                bootstrap.user_names.get("U1").map(String::as_str),
                Some("Interleaved Compatibility Display"),
                "retry must preserve a compatibility write that replaced attempt one"
            );
            assert_eq!(
                bootstrap.user_full_names.get("U1").map(String::as_str),
                Some("Attempt Two Full"),
                "retry must replace an untouched field written by attempt one"
            );
            assert_eq!(
                bootstrap.user_avatar_urls.get("U1").map(String::as_str),
                Some("https://example.test/attempt-two.png")
            );
            assert_eq!(
                bootstrap.user_search_aliases.get("U1"),
                Some(&vec![
                    "Attempt Two Display".to_string(),
                    "Attempt Two Full".to_string(),
                    "attempt-two".to_string(),
                ])
            );
            assert!(
                !bootstrap.user_statuses.contains_key("U1"),
                "retry must apply an explicit clear to a status written by attempt one"
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_repair_retains_journal_batches_across_a_second_reset_before_drain() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-repair-journal-retention-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let initial = SlackConversation {
                id: "C1".into(),
                name: Some("initial".into()),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![initial.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, std::slice::from_ref(&initial))
                .await
                .unwrap();

            let first = workspace
                .apply_and_enqueue(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        name: Some("first pending".into()),
                        ..initial.clone()
                    }),
                )
                .unwrap();
            let second = workspace
                .apply_and_enqueue(
                    Some(&store),
                    MutationOrigin::Realtime,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        name: Some("second pending".into()),
                        ..initial
                    }),
                )
                .unwrap();
            let expected_revisions = vec![first.patch().revision(), second.patch().revision()];

            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();
            {
                let _admission = workspace.publication_admission.lock().await;
                workspace
                    .repair_workspace_cache_admitted(&store)
                    .await
                    .unwrap();
                let pending = workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned");
                assert_eq!(pending.len(), 2);
                assert!(pending.iter().all(|entry| entry.persisted));
                assert!(
                    pending.iter().all(|entry| entry.batch.is_some()),
                    "stable repair must retain replayable batches until recovery-locked drain"
                );
            }

            store.corrupt_conversation_payload("C1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();
            let drained = {
                let _admission = workspace.publication_admission.lock().await;
                workspace
                    .recover_persisted_admitted(Some(&store))
                    .await
                    .unwrap()
            };
            assert_eq!(
                drained
                    .into_writes()
                    .into_iter()
                    .map(|write| write.reduction.patch().revision())
                    .collect::<Vec<_>>(),
                expected_revisions,
                "a second reset must preserve FIFO publication"
            );
            assert!(!store.workspace_cache_needs_repair());
            assert_eq!(
                load_test_conversations(&store).await.unwrap().unwrap()[0]
                    .name
                    .as_deref(),
                Some("second pending")
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_cache_repair_restores_reaction_actor_tombstones() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-reaction-actor-repair-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            };
            let reacted = SlackMessage {
                ts: "1.0".into(),
                text: Some("reacted".into()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".into()),
                    count: Some(2),
                    users: Some(vec!["U_OTHER".into()]),
                }]),
                ..Default::default()
            };
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation.clone()],
                    histories: HashMap::from([("C1".into(), vec![reacted.clone()])]),
                    ..Default::default()
                }),
            );
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![reacted.clone()],
                    },
                ],
            )
            .await
            .unwrap();
            let removal = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "1.0".into(),
                name: "wave".into(),
                user_id: "U_SELF".into(),
                added: false,
            };
            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::Local,
                    WorkspaceMutation::ReactionChanged(removal.clone()),
                )
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();
            assert!(load_test_conversations(&store).await.unwrap().is_none());
            assert!(store.workspace_cache_needs_repair());

            let _admission = workspace.publication_admission.lock().await;
            workspace
                .repair_workspace_cache_admitted(&store)
                .await
                .unwrap();
            assert!(!store.workspace_cache_needs_repair());

            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(bootstrap.reaction_actor_states, vec![removal.clone()]);
            let mut restored = WorkspaceCoordinator::default();
            restored.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                reaction_actor_states: bootstrap.reaction_actor_states,
                ..Default::default()
            }));
            let revision = restored.revision();
            assert!(restored
                .apply(WorkspaceMutation::ReactionChanged(removal))
                .is_none());
            assert_eq!(restored.revision(), revision);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_adapter_orders_cache_web_local_and_realtime_mutations() {
        let workspace = WorkspaceReducerAdapter::default();
        let conversation = SlackConversation {
            id: "C1".into(),
            name: Some("general".into()),
            is_channel: Some(true),
            ..Default::default()
        };
        assert!(workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation.clone()],
                    ..Default::default()
                }),
            )
            .is_some());

        let base_revision = workspace.revision();
        let mut renamed = conversation;
        renamed.name = Some("announcements".into());
        assert!(workspace
            .apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::MembershipSnapshot(SnapshotEnvelope::new(
                    base_revision,
                    ConversationMembershipSnapshot {
                        conversations: vec![renamed],
                        starred_ids: None,
                    },
                )),
            )
            .is_some());

        let posted = SlackMessage {
            ts: "1.000".into(),
            text: Some("sent".into()),
            ..Default::default()
        };
        assert!(workspace
            .apply(
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: posted.clone(),
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
            )
            .is_some());

        let mut edited = posted;
        edited.text = Some("edited".into());
        assert!(workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: edited.clone(),
                    kind: MessageMutationKind::Changed,
                    origin: MutationOrigin::Realtime,
                },
            )
            .is_some());
        assert_eq!(workspace.revision().value(), 4);
        assert!(workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: edited,
                    kind: MessageMutationKind::Changed,
                    origin: MutationOrigin::Realtime,
                },
            )
            .is_none());
        assert_eq!(workspace.revision().value(), 4);
    }

    #[test]
    fn realtime_socket_events_enter_the_workspace_adapter_once() {
        let workspace = WorkspaceReducerAdapter::default();
        assert!(apply_realtime_workspace_event(
            &workspace,
            &SocketModeEvent::Message(Box::new(socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: "1.000".into(),
                    text: Some("hello".into()),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            })),
        )
        .is_some());
        assert!(apply_realtime_workspace_event(
            &workspace,
            &SocketModeEvent::UserChanged(Box::new(SlackUser {
                id: Some("U1".into()),
                name: Some("person".into()),
                ..Default::default()
            })),
        )
        .is_none());
        assert_eq!(workspace.revision().value(), 2);

        assert!(apply_realtime_workspace_event(
            &workspace,
            &SocketModeEvent::UserHuddleChanged(Box::new(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    huddle_state_call_id: Some("R1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        )
        .is_none());
        assert_eq!(workspace.revision().value(), 2);

        assert!(apply_realtime_workspace_event(
            &workspace,
            &SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                channel_id: "C1".into(),
                ts: "1.000".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }),
        )
        .is_none());
        assert_eq!(workspace.revision().value(), 3);
    }

    #[test]
    fn user_status_snapshots_preserve_users_changed_after_the_request_started() {
        let sync = UserStatusSync::default();
        let base_revision = sync.revision();
        sync.publish_change("U_CHANGED", || {});

        let mut preserved = HashSet::new();
        sync.publish_snapshot(base_revision, |user_ids| preserved = user_ids);

        assert_eq!(preserved, HashSet::from(["U_CHANGED".to_string()]));
        assert!(sync.is_user_revision_current("U_CHANGED", 1));
        assert!(sync.is_user_revision_current("U_UNCHANGED", 0));
    }

    #[test]
    fn attention_metrics_count_committed_effects_but_not_persistence_previews() {
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            }),
        );
        let message = SlackMessage {
            ts: "1.000".into(),
            user: Some("U_OTHER".into()),
            text: Some("hello".into()),
            ..Default::default()
        };

        assert!(workspace
            .preview_message_attention(
                "D1",
                &message,
                MessageMutationKind::Posted,
                MutationOrigin::Realtime,
            )
            .is_some());
        assert_eq!(
            workspace.attention_metrics_snapshot().committed_decisions,
            0
        );

        assert!(workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "D1".into(),
                    message,
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                },
            )
            .is_some());
        let metrics = workspace.attention_metrics_snapshot();
        assert_eq!(metrics.committed_decisions, 1);
        assert_eq!(metrics.unread_decisions, 1);
        assert_eq!(metrics.notification_candidates, 1);
        assert_eq!(metrics.reason_count(AttentionReason::DirectMessage), 1);
        assert_eq!(metrics.origin_count(MutationOrigin::Realtime), 1);
        assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1);
    }

    #[test]
    fn preference_change_during_persistence_suppresses_the_stale_notification_claim() {
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "D1".into(),
                is_channel: Some(false),
                is_im: Some(true),
                ..Default::default()
            }),
        );
        let message = SlackMessage {
            ts: "1.000".into(),
            user: Some("U_OTHER".into()),
            text: Some("hello".into()),
            ..Default::default()
        };
        let stale_preview = workspace
            .preview_message_attention(
                "D1",
                &message,
                MessageMutationKind::Posted,
                MutationOrigin::Realtime,
            )
            .expect("direct message should have an attention preview");
        assert!(stale_preview.decision.send_notification);

        workspace.update_attention_preferences(AttentionPreferences {
            direct_messages: false,
            ..AttentionPreferences::default()
        });
        let reduction = workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "D1".into(),
                    message,
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                },
            )
            .expect("message should be applied after persistence");
        let live_attention = reduction.effects().iter().find_map(|effect| match effect {
            WorkspaceEffect::MessageAttention(effect) => Some(effect),
            WorkspaceEffect::ThreadRead(_) => None,
        });

        assert!(live_attention.is_some_and(|effect| effect.decision.record_unread));
        assert!(claimed_notification_candidate(true, live_attention).is_none());
    }

    #[test]
    fn browser_session_realtime_takes_precedence_without_loading_an_app_token() {
        let app_token_loaded = Cell::new(false);
        let browser = socket_mode::SocketModeCredentials::BrowserSession {
            xoxc_token: "xoxc-browser".into(),
            xoxd_token: "xoxd-browser".into(),
            user_agent: None,
        };

        let selected = select_realtime_credentials(Some(browser.clone()), || {
            app_token_loaded.set(true);
            Ok(Some("xapp-unused".into()))
        })
        .expect("browser realtime selection should succeed");

        assert_eq!(selected, Some(browser));
        assert!(!app_token_loaded.get());
    }

    #[test]
    fn app_token_realtime_remains_the_oauth_fallback() {
        let selected = select_realtime_credentials(None, || Ok(Some("xapp-fallback".into())))
            .expect("app-token realtime selection should succeed");

        assert_eq!(
            selected,
            Some(socket_mode::SocketModeCredentials::AppToken(
                "xapp-fallback".into()
            ))
        );
        assert_eq!(
            select_realtime_credentials(None, || Ok(None))
                .expect("missing realtime credentials are valid"),
            None
        );
    }

    #[derive(Clone)]
    struct TraceWriter(Arc<Mutex<Vec<u8>>>);

    // Tracing callsite interest is process-wide, so local subscriber captures
    // must not rebuild it concurrently.
    static TRACE_SUBSCRIBER_TEST_LOCK: Mutex<()> = Mutex::new(());
    const TRACE_TEST_CHILD_ENV: &str = "CONDUIT_TRACE_TEST_CHILD";

    fn run_trace_test_in_isolated_process(test_name: &str) -> bool {
        if std::env::var_os(TRACE_TEST_CHILD_ENV).is_some() {
            return false;
        }
        let output = std::process::Command::new(
            std::env::current_exe().expect("test executable should be available"),
        )
        .arg("--exact")
        .arg(test_name)
        .arg("--test-threads=1")
        .env_clear()
        .env("LANG", "C.UTF-8")
        .env(TRACE_TEST_CHILD_ENV, "1")
        .output()
        .expect("isolated trace test should start");
        assert!(
            output.status.success(),
            "isolated trace test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        true
    }

    impl Write for TraceWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("trace output lock poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn runtime_trace_output_contains_correlation_fields_and_redacts_signed_urls() {
        if run_trace_test_in_isolated_process(
            "runtime::tests::runtime_trace_output_contains_correlation_fields_and_redacts_signed_urls",
        ) {
            return;
        }
        let _trace_guard = TRACE_SUBSCRIBER_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = TraceWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let identity = RuntimeIdentity {
            session: SessionId::default().next(),
            request: RequestId::new(42),
        };
        let command = RuntimeCommand::LoadMedia {
            url: "https://viewer:password@files.slack.com/file?token=signed-secret#preview"
                .to_string(),
            name: "private filename".to_string(),
        };

        tracing::subscriber::with_default(subscriber, || {
            let span = RuntimeTraceFields::for_command(identity, &command).span();
            let _entered = span.enter();
            tracing::debug!(target: "conduit::runtime", event = "scheduled");
        });

        let output = String::from_utf8(output.lock().expect("trace output lock poisoned").clone())
            .expect("trace output should be UTF-8");
        assert!(output.contains("runtime.command"));
        assert!(output.contains("SessionId(1)"));
        assert!(output.contains("RequestId(42)"));
        assert!(output.contains("operation=Media"));
        assert!(output.contains("media:https://files.slack.com/file"));
        for secret in [
            "viewer",
            "password",
            "signed-secret",
            "preview",
            "private filename",
        ] {
            assert!(!output.contains(secret), "trace leaked {secret}: {output}");
        }
    }

    #[test]
    fn attention_traces_contain_only_stable_categories_and_counters() {
        if run_trace_test_in_isolated_process(
            "runtime::tests::attention_traces_contain_only_stable_categories_and_counters",
        ) {
            return;
        }
        let _trace_guard = TRACE_SUBSCRIBER_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = TraceWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_TRACE_SECRET".into()),
        });
        workspace.update_attention_preferences(AttentionPreferences {
            names_and_aliases: vec!["ZirconAliasCanary".into()],
            keywords: vec!["TopSecretKeywordCanary".into()],
            ..AttentionPreferences::default()
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "C_TRACE_SECRET".into(),
                name: Some("PrivateConversationCanary".into()),
                ..Default::default()
            }),
        );

        tracing::subscriber::with_default(subscriber, || {
            workspace.apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C_TRACE_SECRET".into(),
                    message: SlackMessage {
                        ts: "1.000".into(),
                        user: Some("U_OTHER_TRACE_SECRET".into()),
                        text: Some(
                            "<@U_TRACE_SECRET> ZirconAliasCanary TopSecretKeywordCanary".into(),
                        ),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                },
            );
            workspace
                .record_attention_persistence(AttentionPersistenceOutcome::AlreadyObserved, false);
            let metrics = workspace.attention_metrics_handle();
            metrics.record_queue_send(|| Ok::<_, ()>(())).unwrap();
            metrics.dequeue_queue_slot();
            workspace.trace_attention_metrics_snapshot();
        });

        let output = String::from_utf8(output.lock().expect("trace output lock poisoned").clone())
            .expect("trace output should be UTF-8");
        for expected in [
            "attention_decision",
            "attention_persistence",
            "attention_queue_high_water",
            "attention_metrics_snapshot",
            "outcome=\"already_observed\"",
            "origin=\"realtime\"",
            "delivery=\"fresh\"",
            "direct_mention",
            "name_or_alias",
            "keyword_or_phrase",
        ] {
            assert!(
                output.contains(expected),
                "trace omitted {expected}: {output}"
            );
        }
        for private in [
            "U_TRACE_SECRET",
            "U_OTHER_TRACE_SECRET",
            "C_TRACE_SECRET",
            "PrivateConversationCanary",
            "ZirconAliasCanary",
            "TopSecretKeywordCanary",
        ] {
            assert!(
                !output.contains(private),
                "attention trace leaked private input: {output}"
            );
        }
    }

    #[test]
    fn runtime_trace_fields_include_identity_and_context_but_not_payloads() {
        let identity = RuntimeIdentity {
            session: SessionId::default().next(),
            request: RequestId::new(42),
        };
        let command = RuntimeCommand::PostMessage {
            channel_id: "C123".to_string(),
            text: "do not trace this message".to_string(),
            thread_ts: None,
        };

        let fields = RuntimeTraceFields::for_command(identity, &command);

        assert_eq!(fields.session, identity.session);
        assert_eq!(fields.request, identity.request);
        assert_eq!(fields.operation, RuntimeOperation::PostMessage);
        assert_eq!(fields.target, "message:C123:main");
        assert!(!format!("{fields:?}").contains("do not trace this message"));
    }

    #[test]
    fn runtime_failures_map_typed_boundary_categories_to_safe_messages() {
        let auth = anyhow::Error::new(crate::slack::SlackError::Api {
            method: "auth.test".to_string(),
            code: "invalid_auth".to_string(),
        });
        let storage = anyhow::Error::new(crate::store::StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret cache path",
        )));
        let permission = anyhow::Error::new(crate::slack::SlackError::Api {
            method: "conversations.invite".to_string(),
            code: "no_permission".to_string(),
        });

        let auth = RuntimeFailure::from_error(&auth);
        let storage = RuntimeFailure::from_error(&storage);
        let permission = RuntimeFailure::from_error(&permission);

        assert_eq!(auth.category, RuntimeFailureCategory::Authentication);
        assert_eq!(auth.message, "Slack authentication failed. Sign in again.");
        assert_eq!(storage.category, RuntimeFailureCategory::Storage);
        assert_eq!(storage.message, "Conduit could not access its local data.");
        assert!(!storage.message.contains("secret cache path"));
        assert_eq!(permission.category, RuntimeFailureCategory::Validation);
        assert_eq!(
            permission.message,
            "Slack does not allow this action for this conversation."
        );
    }

    #[test]
    fn runtime_failures_map_rate_limits_validation_and_unknown_errors() {
        let rate_limit = anyhow::Error::new(crate::slack::SlackError::RateLimited {
            method: "conversations.history".to_string(),
        });
        let validation = RuntimeFailure::validation("Enter both browser-session tokens");
        let unknown = RuntimeFailure::from_error(&anyhow::anyhow!("sensitive internals"));

        assert_eq!(
            RuntimeFailure::from_error(&rate_limit).category,
            RuntimeFailureCategory::RateLimited
        );
        assert_eq!(validation.category, RuntimeFailureCategory::Validation);
        assert_eq!(validation.message, "Enter both browser-session tokens");
        assert_eq!(unknown.category, RuntimeFailureCategory::Internal);
        assert_eq!(unknown.message, "Conduit encountered an unexpected error.");
    }

    #[test]
    fn browser_session_connectivity_failure_explains_user_agent_recovery() {
        let timeout = anyhow::Error::new(crate::slack::SlackError::from(anyhow::Error::new(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"),
        )));

        let failure =
            authentication_failure(AuthenticationFailureContext::BrowserSession, &timeout);

        assert_eq!(failure.category, RuntimeFailureCategory::Network);
        assert!(failure.message.contains("exact User-Agent"));
        assert!(failure.message.contains("XOXC/XOXD"));
        assert!(failure.message.contains("use OAuth"));
        assert!(failure.message.contains("TLS fingerprint"));
        assert!(!failure.message.contains("request timed out"));
    }

    struct CancellationSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for CancellationSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn background_work_does_not_block_later_interactive_work() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
            let (background_started_tx, background_started_rx) = tokio::sync::oneshot::channel();
            let background_gate = Arc::new(tokio::sync::Notify::new());
            let background_task = tokio::spawn({
                let limits = limits.clone();
                let background_gate = Arc::clone(&background_gate);
                async move {
                    let _permit = limits.acquire(RuntimeTaskLane::Background).await;
                    let _ = background_started_tx.send(());
                    background_gate.notified().await;
                }
            });

            background_started_rx
                .await
                .expect("background task did not start");
            let interactive_permit = tokio::time::timeout(
                Duration::from_millis(100),
                limits.acquire(RuntimeTaskLane::Interactive),
            )
            .await;

            assert!(
                interactive_permit.is_ok(),
                "interactive work was blocked by background work"
            );
            background_task.abort();
        });
    }

    #[test]
    fn image_work_does_not_block_later_upload_work() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
            let (image_started_tx, image_started_rx) = tokio::sync::oneshot::channel();
            let image_gate = Arc::new(tokio::sync::Notify::new());
            let image_task = tokio::spawn({
                let limits = limits.clone();
                let image_gate = Arc::clone(&image_gate);
                async move {
                    let _permit = limits.acquire(RuntimeTaskLane::Image).await;
                    let _ = image_started_tx.send(());
                    image_gate.notified().await;
                }
            });

            image_started_rx.await.expect("image task did not start");
            let upload_permit = tokio::time::timeout(
                Duration::from_millis(100),
                limits.acquire(RuntimeTaskLane::Upload),
            )
            .await;

            assert!(
                upload_permit.is_ok(),
                "upload work was blocked by image work"
            );
            image_task.abort();
        });
    }

    #[test]
    fn navigation_work_does_not_block_behind_interactive_mutation() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
            let interactive_permit = limits.acquire(RuntimeTaskLane::Interactive).await;
            let navigation_permit = tokio::time::timeout(
                Duration::from_millis(100),
                limits.acquire(RuntimeTaskLane::Navigation),
            )
            .await;

            assert!(
                navigation_permit.is_ok(),
                "navigation work was blocked by an interactive mutation"
            );
            drop(interactive_permit);
        });
    }

    #[test]
    fn switching_main_navigation_cancels_scheduled_old_target_and_starts_new_target() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let state = Arc::new(Mutex::new(RuntimeState::new(session)));
            let first_command = RuntimeCommand::LoadHistory {
                channel_id: "C1".to_string(),
            };
            let second_command = RuntimeCommand::LoadHistory {
                channel_id: "C2".to_string(),
            };
            let first_identity = RuntimeIdentity {
                session,
                request: RequestId::new(1),
            };
            let second_identity = RuntimeIdentity {
                session,
                request: RequestId::new(2),
            };
            let first_request = TrackedRequest::for_command(first_identity, &first_command);
            let second_request = TrackedRequest::for_command(second_identity, &second_command);
            let first_started = Arc::new(tokio::sync::Notify::new());
            let first_started_for_work = Arc::clone(&first_started);
            let first_work = RuntimeSyncWork::new(move |_attempt| {
                let first_started = Arc::clone(&first_started_for_work);
                async move {
                    first_started.notify_one();
                    future::pending::<JobOutcome>().await
                }
            });
            let first_receipt = match state.lock().unwrap().admit_sync_request(
                &first_request,
                connected_command_sync_plan(&first_command).unwrap(),
                first_work,
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("first scheduled navigation was not admitted"),
            };
            first_started.notified().await;

            let second_started = Arc::new(tokio::sync::Notify::new());
            let second_started_for_work = Arc::clone(&second_started);
            let second_work = RuntimeSyncWork::new(move |_attempt| {
                let second_started = Arc::clone(&second_started_for_work);
                async move {
                    second_started.notify_one();
                    JobOutcome::Succeeded
                }
            });
            let second_receipt = match state.lock().unwrap().admit_sync_request(
                &second_request,
                connected_command_sync_plan(&second_command).unwrap(),
                second_work,
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("second scheduled navigation was not admitted"),
            };
            let second_admission = second_receipt.admission();

            let first_terminal =
                tokio::time::timeout(Duration::from_millis(100), first_receipt.wait())
                    .await
                    .expect("abandoned navigation was not cancelled")
                    .expect("first navigation receipt was lost");
            assert_eq!(
                first_terminal.result(),
                RuntimeSyncTerminalResult::Cancelled
            );
            tokio::time::timeout(Duration::from_millis(100), second_started.notified())
                .await
                .expect("new navigation did not get capacity");
            let second_terminal =
                tokio::time::timeout(Duration::from_millis(100), second_receipt.wait())
                    .await
                    .expect("new navigation did not complete")
                    .expect("second navigation receipt was lost");
            assert_eq!(
                second_terminal.result(),
                RuntimeSyncTerminalResult::Succeeded
            );

            let mut runtime_state = state.lock().unwrap();
            runtime_state.finish_sync_request(
                session,
                first_terminal.admission(),
                Some(&first_request),
            );
            assert!(runtime_state
                .active_navigation
                .get(&NavigationSlot::Main)
                .is_some_and(|active| {
                    matches!(
                        active,
                        ActiveWork::Sync { admission, .. }
                            if *admission == second_admission
                    )
                }));
            runtime_state.finish_sync_request(
                session,
                second_terminal.admission(),
                Some(&second_request),
            );
            assert!(!runtime_state
                .active_navigation
                .contains_key(&NavigationSlot::Main));
        });
    }

    #[test]
    fn scheduled_thread_navigation_remains_independent_from_main_navigation() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let state = Arc::new(Mutex::new(RuntimeState::new(session)));
            let main_command = RuntimeCommand::LoadHistory {
                channel_id: "C1".to_string(),
            };
            let thread_command = RuntimeCommand::LoadThread {
                channel_id: "C1".to_string(),
                ts: "1710000000.000100".to_string(),
            };
            let main_request = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                &main_command,
            );
            let thread_request = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                &thread_command,
            );
            let main_started = Arc::new(tokio::sync::Notify::new());
            let main_release = Arc::new(tokio::sync::Notify::new());
            let main_started_for_work = Arc::clone(&main_started);
            let main_release_for_work = Arc::clone(&main_release);
            let (cancelled_tx, mut cancelled_rx) = oneshot::channel();
            let cancellation_sender = Arc::new(Mutex::new(Some(cancelled_tx)));
            let cancellation_sender_for_work = Arc::clone(&cancellation_sender);
            let main_receipt = match state.lock().unwrap().admit_sync_request(
                &main_request,
                connected_command_sync_plan(&main_command).unwrap(),
                RuntimeSyncWork::new(move |_attempt| {
                    let main_started = Arc::clone(&main_started_for_work);
                    let main_release = Arc::clone(&main_release_for_work);
                    let cancellation_sender = Arc::clone(&cancellation_sender_for_work);
                    async move {
                        let _cancellation = CancellationSignal(
                            cancellation_sender
                                .lock()
                                .expect("cancellation sender lock poisoned")
                                .take(),
                        );
                        main_started.notify_one();
                        main_release.notified().await;
                        JobOutcome::Succeeded
                    }
                }),
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("main navigation was not admitted"),
            };
            main_started.notified().await;

            let thread_started = Arc::new(tokio::sync::Notify::new());
            let thread_started_for_work = Arc::clone(&thread_started);
            let thread_receipt = match state.lock().unwrap().admit_sync_request(
                &thread_request,
                connected_command_sync_plan(&thread_command).unwrap(),
                RuntimeSyncWork::new(move |_attempt| {
                    let thread_started = Arc::clone(&thread_started_for_work);
                    async move {
                        thread_started.notify_one();
                        JobOutcome::Succeeded
                    }
                }),
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("thread navigation was not admitted"),
            };

            tokio::time::timeout(Duration::from_millis(100), thread_started.notified())
                .await
                .expect("thread navigation did not start independently");
            assert_eq!(
                thread_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            assert!(matches!(
                cancelled_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));

            main_release.notify_one();
            assert_eq!(
                main_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
        });
    }

    #[test]
    fn membership_invalidation_at_capacity_is_retained_and_readmitted() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let state = Arc::new(Mutex::new(RuntimeState::new(session)));
            state.lock().unwrap().sync_scheduler = RuntimeSyncScheduler::new(
                SchedulerConfig::new(1, 1, 1).expect("valid test scheduler configuration"),
            );

            let blocker_started = Arc::new(tokio::sync::Notify::new());
            let blocker_release = Arc::new(tokio::sync::Notify::new());
            let blocker_started_for_work = Arc::clone(&blocker_started);
            let blocker_release_for_work = Arc::clone(&blocker_release);
            schedule_session_sync_work(
                &state,
                session,
                startup_sync_plan(RuntimeStartupSyncKind::EmojiCatalog),
                RuntimeSyncWork::new(move |_attempt| {
                    let blocker_started = Arc::clone(&blocker_started_for_work);
                    let blocker_release = Arc::clone(&blocker_release_for_work);
                    async move {
                        blocker_started.notify_one();
                        blocker_release.notified().await;
                        JobOutcome::Succeeded
                    }
                }),
            );
            blocker_started.notified().await;

            let membership_started = Arc::new(tokio::sync::Notify::new());
            let membership_started_for_work = Arc::clone(&membership_started);
            schedule_session_sync_work(
                &state,
                session,
                startup_sync_plan(RuntimeStartupSyncKind::Membership),
                RuntimeSyncWork::new(move |_attempt| {
                    let membership_started = Arc::clone(&membership_started_for_work);
                    async move {
                        membership_started.notify_one();
                        JobOutcome::Succeeded
                    }
                }),
            );

            assert!(state.lock().unwrap().pending_membership.is_some());
            assert!(
                tokio::time::timeout(Duration::from_millis(20), membership_started.notified())
                    .await
                    .is_err(),
                "membership work started before scheduler capacity was released"
            );

            blocker_release.notify_one();
            tokio::time::timeout(Duration::from_millis(100), membership_started.notified())
                .await
                .expect("retained membership invalidation was not readmitted");
            tokio::task::yield_now().await;
            assert!(state.lock().unwrap().pending_membership.is_none());
        });
    }

    #[test]
    fn repeated_manual_membership_refresh_queues_without_cancelling_running_work() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let state = Arc::new(Mutex::new(RuntimeState::new(session)));
            let command = RuntimeCommand::RefreshConversations;
            let plan = connected_command_sync_plan(&command).unwrap();
            let first_request = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                &command,
            );
            let second_request = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                &command,
            );
            let first_started = Arc::new(tokio::sync::Notify::new());
            let first_release = Arc::new(tokio::sync::Notify::new());
            let first_started_for_work = Arc::clone(&first_started);
            let first_release_for_work = Arc::clone(&first_release);
            let (cancelled_tx, mut cancelled_rx) = oneshot::channel();
            let cancellation_sender = Arc::new(Mutex::new(Some(cancelled_tx)));
            let cancellation_sender_for_work = Arc::clone(&cancellation_sender);
            let first_receipt = match state.lock().unwrap().admit_sync_request(
                &first_request,
                plan,
                RuntimeSyncWork::new(move |_attempt| {
                    let first_started = Arc::clone(&first_started_for_work);
                    let first_release = Arc::clone(&first_release_for_work);
                    let cancellation_sender = Arc::clone(&cancellation_sender_for_work);
                    async move {
                        let _cancellation = CancellationSignal(
                            cancellation_sender
                                .lock()
                                .expect("cancellation sender lock poisoned")
                                .take(),
                        );
                        first_started.notify_one();
                        first_release.notified().await;
                        JobOutcome::Succeeded
                    }
                }),
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("first membership refresh was not admitted"),
            };
            first_started.notified().await;

            let second_started = Arc::new(tokio::sync::Notify::new());
            let second_started_for_work = Arc::clone(&second_started);
            let second_receipt = match state.lock().unwrap().admit_sync_request(
                &second_request,
                plan,
                RuntimeSyncWork::new(move |_attempt| {
                    let second_started = Arc::clone(&second_started_for_work);
                    async move {
                        second_started.notify_one();
                        JobOutcome::Succeeded
                    }
                }),
            ) {
                RuntimeSyncRequestAdmission::Accepted(receipt) => receipt,
                _ => panic!("second membership refresh was not admitted"),
            };

            tokio::task::yield_now().await;
            assert!(matches!(
                cancelled_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), second_started.notified())
                    .await
                    .is_err(),
                "same-target membership refresh ran concurrently"
            );

            first_release.notify_one();
            assert_eq!(
                first_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            tokio::time::timeout(Duration::from_millis(100), second_started.notified())
                .await
                .expect("queued membership refresh did not start");
            assert_eq!(
                second_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
        });
    }

    #[test]
    fn mutations_are_session_tracked_without_same_context_supersession() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let command = RuntimeCommand::PostMessage {
                channel_id: "C1".to_string(),
                text: "hello".to_string(),
                thread_ts: None,
            };
            let first = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                &command,
            );
            let second = TrackedRequest::for_command(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                &command,
            );
            let first_task = tokio::spawn(future::pending::<()>());
            let second_task = tokio::spawn(future::pending::<()>());
            let mut state = RuntimeState::new(session);

            assert!(state.register_task(
                session,
                1,
                Some(first.clone()),
                first_task.abort_handle(),
            ));
            assert!(state.register_task(
                session,
                2,
                Some(second.clone()),
                second_task.abort_handle(),
            ));
            assert!(!first_task.is_finished());
            assert!(!second_task.is_finished());
            assert!(state.active_requests.is_empty());

            state.finish_task(1, Some(&first));
            state.finish_task(2, Some(&second));
            first_task.abort();
            second_task.abort();
        });
    }

    #[test]
    fn only_read_commands_supersede_previous_requests() {
        assert!(RuntimeCommand::SearchMessages {
            query: "hello".to_string(),
        }
        .supersedes_previous());
        assert!(!RuntimeCommand::SetSaved {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
            add: true,
            thread_ts: None,
        }
        .supersedes_previous());
        assert!(!RuntimeCommand::UploadFile {
            channel_id: "C1".to_string(),
            thread_ts: None,
            path: PathBuf::from("example.txt"),
            initial_comment: None,
            remove_after_upload: false,
        }
        .supersedes_previous());
    }

    #[test]
    fn superseded_request_cleanup_does_not_remove_newer_request() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let context = OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace);
            let first = TrackedRequest::new(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                context.clone(),
            );
            let second = TrackedRequest::new(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                context.clone(),
            );
            let old_task = tokio::spawn(future::pending::<()>());
            let new_task = tokio::spawn(future::pending::<()>());
            let mut state = RuntimeState::new(session);

            state.register_task(session, 1, Some(first.clone()), old_task.abort_handle());
            state.register_task(session, 2, Some(second.clone()), new_task.abort_handle());
            let old_result = tokio::time::timeout(Duration::from_millis(100), old_task)
                .await
                .expect("superseded task was not aborted")
                .expect_err("superseded task completed normally");
            assert!(old_result.is_cancelled());

            state.finish_task(1, Some(&first));
            assert_eq!(
                state.active_requests.get(&context),
                Some(&ActiveWork::Task { task_id: 2 })
            );

            state.finish_task(2, Some(&second));
            assert!(!state.active_requests.contains_key(&context));
            new_task.abort();
        });
    }

    #[test]
    fn completed_newer_request_still_rejects_older_background_request() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let context =
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace);
            let newer = TrackedRequest::new(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                context.clone(),
            );
            let older = TrackedRequest::new(
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                context.clone(),
            );
            let newer_task = tokio::spawn(future::pending::<()>());
            let older_task = tokio::spawn(future::pending::<()>());
            let mut state = RuntimeState::new(session);

            assert!(state.register_task(
                session,
                2,
                Some(newer.clone()),
                newer_task.abort_handle(),
            ));
            state.finish_task(2, Some(&newer));
            assert!(!state.register_task(session, 1, Some(older), older_task.abort_handle(),));
            let older_result = tokio::time::timeout(Duration::from_millis(100), older_task)
                .await
                .expect("older background task was not aborted")
                .expect_err("older background task completed normally");

            assert!(older_result.is_cancelled());
            assert_eq!(
                state.latest_requests.get(&context),
                Some(&RequestId::new(2))
            );
            newer_task.abort();
        });
    }

    #[test]
    fn sign_out_still_completes_when_keyring_clear_fails() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let (sender, mut events) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SignOut, RuntimeTarget::Workspace),
            );

            finish_sign_out(&event_sender, Err(anyhow!("keyring unavailable")));

            assert!(matches!(
                events.recv().await.map(|event| event.kind),
                Some(RuntimeEventKind::Error(_))
            ));
            assert!(matches!(
                events.recv().await.map(|event| event.kind),
                Some(RuntimeEventKind::WorkspaceLifecycle(
                    WorkspaceLifecycleEvent::SignedOut
                ))
            ));
            assert!(matches!(
                events.recv().await.map(|event| event.kind),
                Some(RuntimeEventKind::SignedOut)
            ));
        });
    }

    #[test]
    fn posted_message_is_visible_before_persistence_completes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let (sender, mut events) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::PostMessage,
                    RuntimeTarget::Message {
                        channel_id: "C1".into(),
                        thread_ts: None,
                    },
                ),
            );
            let message = SlackMessage {
                ts: "1.000".into(),
                text: Some("sent".into()),
                ..SlackMessage::default()
            };
            let (persistence_started, persistence_started_rx) = oneshot::channel();
            let (release_persistence, release_persistence_rx) = oneshot::channel();

            let publish = tokio::spawn(publish_posted_message_before_persistence(
                event_sender,
                "C1".into(),
                message.clone(),
                async move {
                    let _ = persistence_started.send(());
                    let _ = release_persistence_rx.await;
                },
            ));

            tokio::time::timeout(Duration::from_secs(1), persistence_started_rx)
                .await
                .expect("persistence did not start")
                .expect("persistence start signal was dropped");
            assert!(
                !publish.is_finished(),
                "publish task completed while persistence was blocked"
            );

            let event = events
                .try_recv()
                .expect("message event should be queued before persistence completes");
            let RuntimeEventKind::MessagePosted {
                channel_id,
                message: posted,
            } = event.kind
            else {
                panic!("expected posted message event");
            };
            assert_eq!(channel_id, "C1");
            assert_eq!(*posted, message);

            release_persistence
                .send(())
                .expect("persistence task should still be waiting");
            publish.await.expect("publish task failed");
        });
    }

    #[test]
    fn lifecycle_failure_event_distinguishes_authentication_from_retryable_failures() {
        assert_eq!(
            lifecycle_failure_event(&RuntimeFailure {
                category: RuntimeFailureCategory::Authentication,
                message: "safe".into(),
            }),
            WorkspaceLifecycleEvent::AuthenticationFailed
        );
        for category in [
            RuntimeFailureCategory::Network,
            RuntimeFailureCategory::RateLimited,
            RuntimeFailureCategory::Storage,
            RuntimeFailureCategory::Validation,
            RuntimeFailureCategory::Internal,
        ] {
            assert_eq!(
                lifecycle_failure_event(&RuntimeFailure {
                    category,
                    message: "safe".into(),
                }),
                WorkspaceLifecycleEvent::RetryableFailure
            );
        }
    }

    #[test]
    fn lifecycle_runtime_event_has_workspace_operation_context() {
        let fallback = OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C1".into()),
        );

        assert_eq!(
            RuntimeEventKind::WorkspaceLifecycle(WorkspaceLifecycleEvent::RecoveryStarted)
                .operation_context(&fallback),
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
        );
    }

    #[test]
    fn canonical_history_page_projection_is_page_scoped_and_authoritative() {
        let canonical_edit = SlackMessage {
            ts: "1".into(),
            client_msg_id: Some("edit".into()),
            text: Some("canonical edit".into()),
            ..SlackMessage::default()
        };
        let canonical_identity = SlackMessage {
            ts: "6".into(),
            client_msg_id: Some("same".into()),
            text: Some("canonical identity".into()),
            ..SlackMessage::default()
        };
        let moved_to_thread = SlackMessage {
            ts: "3".into(),
            client_msg_id: Some("moved".into()),
            thread_ts: Some("root".into()),
            text: Some("moved".into()),
            ..SlackMessage::default()
        };
        let last = SlackMessage {
            ts: "7".into(),
            text: Some("last".into()),
            ..SlackMessage::default()
        };
        let requested = vec![
            SlackMessage {
                text: Some("stale edit".into()),
                ..canonical_edit.clone()
            },
            SlackMessage {
                ts: "2".into(),
                text: Some("deleted".into()),
                ..SlackMessage::default()
            },
            SlackMessage {
                thread_ts: None,
                text: Some("stale location".into()),
                ..moved_to_thread.clone()
            },
            SlackMessage {
                ts: "5".into(),
                thread_ts: Some("root".into()),
                text: Some("normal reply".into()),
                ..SlackMessage::default()
            },
            SlackMessage {
                ts: "temporary".into(),
                text: Some("temporary identity".into()),
                ..canonical_identity.clone()
            },
            canonical_edit.clone(),
            last.clone(),
        ];

        let projected = canonical_history_page_projection(
            &[
                canonical_edit.clone(),
                canonical_identity.clone(),
                moved_to_thread,
                last.clone(),
            ],
            &requested,
        );

        assert_eq!(
            projected,
            vec![canonical_edit, canonical_identity, last],
            "projection must preserve requested order, deduplicate, and reject deleted or moved messages"
        );
    }

    #[test]
    fn fresh_history_projection_keeps_thirty_item_page_and_concurrent_post() {
        let mut requested = (1..=30)
            .rev()
            .map(|index| SlackMessage {
                ts: format!("{index:02}.0"),
                text: Some(format!("page {index}")),
                ..SlackMessage::default()
            })
            .collect::<Vec<_>>();
        let concurrent = SlackMessage {
            ts: "31.0".into(),
            text: Some("concurrent".into()),
            ..SlackMessage::default()
        };
        let older_cached = SlackMessage {
            ts: "00.5".into(),
            text: Some("older cached".into()),
            ..SlackMessage::default()
        };
        let canonical = requested
            .iter()
            .cloned()
            .map(|message| (message, WorkspaceRevision::INITIAL))
            .chain([
                (concurrent.clone(), WorkspaceRevision::INITIAL.successor()),
                (older_cached.clone(), WorkspaceRevision::INITIAL),
            ])
            .collect::<Vec<_>>();
        requested.push(SlackMessage {
            ts: "00.0".into(),
            thread_ts: Some("99.0".into()),
            text: Some("anomalous reply in history response".into()),
            ..SlackMessage::default()
        });

        let paginated = canonical_history_refresh_projection(
            &canonical,
            &requested,
            false,
            WorkspaceRevision::INITIAL,
        );
        assert_eq!(paginated.len(), 31);
        assert_eq!(paginated.first(), Some(&concurrent));
        assert_eq!(paginated.last().unwrap().ts, "01.0");
        assert!(!paginated.contains(&older_cached));

        let complete = canonical_history_refresh_projection(
            &canonical,
            &requested,
            true,
            WorkspaceRevision::INITIAL,
        );
        assert_eq!(complete.len(), 32);
        assert_eq!(complete.first(), Some(&concurrent));
        assert_eq!(complete.last(), Some(&older_cached));
    }

    #[test]
    fn fresh_history_completion_keeps_older_timestamp_post_after_request_base() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");
        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::History,
                    RuntimeTarget::Channel("C1".into()),
                ),
            );
            let requested = vec![
                SlackMessage {
                    ts: "10.0".into(),
                    text: Some("newest requested".into()),
                    ..Default::default()
                },
                SlackMessage {
                    ts: "09.0".into(),
                    text: Some("oldest requested".into()),
                    ..Default::default()
                },
            ];
            workspace
                .apply_persisted_and_publish(
                    None,
                    &events,
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![SlackConversation {
                            id: "C1".into(),
                            ..Default::default()
                        }],
                        histories: HashMap::from([("C1".into(), requested.clone())]),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let request_base = workspace.revision();
            workspace
                .apply_persisted_and_publish(
                    None,
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "01.0".into(),
                            user: Some("U_SELF".into()),
                            text: Some("delayed socket post".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));

            publish_history_snapshot_with_completion(
                &events,
                &None,
                &workspace,
                "C1",
                MutationOrigin::WebApi,
                request_base,
                requested,
                true,
                Some("older".into()),
                false,
                false,
                false,
            )
            .await
            .unwrap();
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoaded {
                messages,
                has_more: true,
                next_cursor: Some(cursor),
                ..
            } = completion.kind
            else {
                panic!("fresh history completion was not queued");
            };
            assert_eq!(cursor, "older");
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["10.0", "09.0", "01.0"]
            );
        });
    }

    #[test]
    fn interactive_history_has_no_legacy_store_or_attention_bypasses() {
        let runtime_source = include_str!("runtime.rs");
        let production = runtime_source
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let command_handler = production
            .split_once("async fn handle_command(")
            .unwrap()
            .1
            .split_once("\nfn socket_membership_refresh_required(")
            .unwrap()
            .0;
        let history_commands = command_handler
            .split_once("RuntimeCommand::LoadHistory")
            .unwrap()
            .1
            .split_once("RuntimeCommand::LoadThread")
            .unwrap()
            .0;
        let (latest, _) = history_commands
            .split_once("RuntimeCommand::LoadOlderHistory")
            .unwrap();
        let cache_read = latest.find("service.load_cached").unwrap();
        let loading_status = latest
            .find("send_status(\"Loading conversation\")")
            .unwrap();
        let network_base = latest.find("let base_revision").unwrap();
        let network_fetch = latest.find("service.fetch").unwrap();
        assert!(cache_read < loading_status);
        assert!(loading_status < network_base);
        assert!(network_base < network_fetch);
        assert!(!history_commands.contains("persist_snapshot_attention"));
        assert!(!history_commands.contains("store_merged_history"));
        assert!(!history_commands.contains("observe_thread_history("));
        assert!(!history_commands.contains("context.workspace.apply("));
        assert_eq!(
            history_commands
                .matches("publish_history_snapshot_with_completion(")
                .count(),
            3
        );
        let durable_publication = production
            .split_once("async fn apply_persisted_and_publish_admitted(")
            .unwrap()
            .1
            .split_once("async fn repair_workspace_cache_admitted(")
            .unwrap()
            .0;
        assert!(durable_publication.contains("publish_persisted_workspace_write(events, write)"));

        let service_source = include_str!("services/conversation_history.rs");
        let service_production = service_source.split_once("#[cfg(test)]").unwrap().0;
        assert!(!service_production.contains("async fn store_history"));
        assert!(!service_production.contains(".store_history("));
        assert!(!service_production.contains("CacheWriteFailed"));
    }

    #[test]
    fn interactive_thread_snapshots_have_no_legacy_store_or_attention_bypasses() {
        let runtime_source = include_str!("runtime.rs");
        let production = runtime_source
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        let command_handler = production
            .split_once("async fn handle_command(")
            .unwrap()
            .1
            .split_once("\nfn socket_membership_refresh_required(")
            .unwrap()
            .0;
        let thread_commands = command_handler
            .split_once("RuntimeCommand::LoadThread")
            .unwrap()
            .1
            .split_once("RuntimeCommand::LoadMessageContext")
            .unwrap()
            .0;
        let (latest, older) = thread_commands
            .split_once("RuntimeCommand::LoadOlderThread")
            .unwrap();
        assert!(!latest.contains("observe_thread_page("));
        assert!(!older.contains("observe_thread_page("));
        assert!(older.contains("older_thread_snapshot_page("));
        assert!(!thread_commands.contains(".store_thread("));
        assert!(!thread_commands.contains(".store_merged_thread("));
        assert!(!thread_commands.contains("persist_snapshot_attention"));
        assert!(!thread_commands.contains("context.workspace.apply("));
        assert_eq!(
            thread_commands
                .matches("publish_thread_snapshot_with_completion(")
                .count(),
            1
        );
        assert_eq!(
            thread_commands
                .matches("publish_thread_snapshot_page_with_completion(")
                .count(),
            1
        );

        let cached_thread = production
            .split_once("async fn load_cached_thread(")
            .unwrap()
            .1
            .split_once("fn require_slack(")
            .unwrap()
            .0;
        assert_eq!(
            cached_thread
                .matches("publish_thread_snapshot_page_with_completion(")
                .count(),
            1
        );
        assert!(!cached_thread.contains("workspace.apply("));
        assert!(!cached_thread.contains("persist_snapshot_attention"));
        assert!(!cached_thread.contains(".store_thread("));
    }

    async fn assert_history_completion_serializes_direct_producer(fallback: bool) {
        let workspace = WorkspaceReducerAdapter::default();
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "C1".into(),
                ..Default::default()
            }),
        );
        let base_revision = workspace.revision();
        let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
        let events = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        let (gate_started, gate_started_rx) = std::sync::mpsc::channel();
        let (release_gate, release_gate_rx) = std::sync::mpsc::channel();
        workspace.set_history_completion_send_gate(Arc::new(TestWorkspacePatchSendGate {
            started: gate_started,
            release: Mutex::new(release_gate_rx),
        }));

        let history_events = events.clone();
        let history_workspace = workspace.clone();
        let history = tokio::spawn(async move {
            publish_history_snapshot_with_completion(
                &history_events,
                &None,
                &history_workspace,
                "C1",
                MutationOrigin::WebApi,
                base_revision,
                vec![SlackMessage {
                    ts: "1.0".into(),
                    text: Some("history".into()),
                    ..Default::default()
                }],
                false,
                None,
                true,
                false,
                false,
            )
            .await
            .unwrap();
        });
        gate_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("history completion did not reach its publication barrier");

        let producer_workspace = workspace.clone();
        let producer_events = events.clone();
        let (producer_attempted, producer_attempted_rx) = std::sync::mpsc::channel();
        let (producer_completed, producer_completed_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            producer_attempted.send(()).unwrap();
            let message = SlackMessage {
                ts: "2.0".into(),
                user: Some("U_SELF".into()),
                text: Some("direct producer".into()),
                ..Default::default()
            };
            if fallback {
                let event = SocketModeEvent::Message(Box::new(
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message,
                        kind: SocketModeMessageKind::Posted,
                    },
                ));
                let attention = apply_realtime_workspace_event(&producer_workspace, &event)
                    .map(|effect| effect.decision);
                producer_events.send_event(RuntimeEventKind::SocketModeEvent { event, attention });
            } else {
                producer_workspace.apply(
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: message.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                );
                producer_events.send_event(RuntimeEventKind::MessagePosted {
                    channel_id: "C1".into(),
                    message: Box::new(message),
                });
            }
            producer_completed.send(()).unwrap();
        });
        producer_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("direct producer did not start");
        let producer_overtook_completion = producer_completed_rx
            .recv_timeout(Duration::from_millis(50))
            .is_ok();

        release_gate.send(()).unwrap();
        history.await.unwrap();
        producer.join().unwrap();
        assert!(
            !producer_overtook_completion,
            "direct producer entered after the canonical clone but before HistoryLoaded"
        );

        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let history_position = delivered
            .iter()
            .position(|event| matches!(event, RuntimeEventKind::HistoryLoaded { .. }))
            .expect("history completion was not queued");
        let producer_position = delivered
            .iter()
            .position(|event| {
                if fallback {
                    matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(_),
                            ..
                        }
                    )
                } else {
                    matches!(event, RuntimeEventKind::MessagePosted { .. })
                }
            })
            .expect("direct producer compatibility event was not queued");
        assert!(history_position < producer_position);
    }

    #[test]
    fn history_clone_and_completion_are_atomic_against_direct_message_producers() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to build test runtime");
        runtime.block_on(async {
            assert_history_completion_serializes_direct_producer(false).await;
            assert_history_completion_serializes_direct_producer(true).await;
        });
    }

    #[test]
    fn realtime_status_persistence_skips_superseded_user_updates() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-status-persistence-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let (runtime_events, _receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let workspace = WorkspaceReducerAdapter::default();
            let sync = UserStatusSync::default();
            let old_revision = sync.publish_change("U1", || {});
            let current_revision = sync.publish_change("U1", || {});
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace,
                sync,
            ));
            let status_user = |text: &str| {
                Box::new(SlackUser {
                    id: Some("U1".into()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some(text.to_string()),
                        status_emoji: Some(String::new()),
                        status_expiration: Some(0),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            };
            sender
                .send(RealtimePersistenceEvent::UserChanged {
                    user: status_user("Current"),
                    status_revision: Some(current_revision),
                })
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::UserChanged {
                    user: status_user("Stale"),
                    status_revision: Some(old_revision),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let statuses = store.load_user_statuses().await.unwrap();
            assert_eq!(
                statuses.get("U1").map(|status| status.text.as_str()),
                Some("Current")
            );
            std::fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn socket_message_deltas_wait_for_store_admission_and_persist_post_edit_delete() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-message-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let admission = workspace.publication_admission.lock().await;
            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));

            let socket_message = |ts: &str, text: &str, kind| RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: ts.into(),
                        user: Some("U_OTHER".into()),
                        text: Some(text.into()),
                        ..Default::default()
                    },
                    kind,
                }),
            };
            sender
                .send(socket_message(
                    "1.0",
                    "original",
                    SocketModeMessageKind::Posted,
                ))
                .unwrap();
            sender
                .send(socket_message(
                    "1.0",
                    "edited",
                    SocketModeMessageKind::Changed,
                ))
                .unwrap();
            sender
                .send(socket_message(
                    "1.0",
                    "deleted",
                    SocketModeMessageKind::Deleted,
                ))
                .unwrap();
            sender
                .send(socket_message(
                    "2.0",
                    "survives",
                    SocketModeMessageKind::Posted,
                ))
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), runtime_receiver.recv())
                    .await
                    .is_err(),
                "socket message authority must wait behind an admitted history mutation"
            );
            drop(admission);
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .filter_map(|event| match event.kind {
                    RuntimeEventKind::SocketModeEvent {
                        event: SocketModeEvent::Message(message),
                        ..
                    } => Some(message.kind),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                delivered,
                [
                    SocketModeMessageKind::Posted,
                    SocketModeMessageKind::Changed,
                    SocketModeMessageKind::Deleted,
                    SocketModeMessageKind::Posted,
                ],
                "compatibility UI events must retain socket FIFO order"
            );

            let stored = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(
                stored
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("2.0", "survives".into())]
            );
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("2.0", "survives".into())]
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn socket_attention_flushes_pending_read_before_notification_acceptance() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-attention-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            store
                .install_conversation_batch_failure_trigger_for("D1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &event_sender,
                    MutationOrigin::Local,
                    WorkspaceMutation::ReadAdvanced {
                        channel_id: "D1".into(),
                        ts: "20.0".into(),
                        remaining_unread: 0,
                    },
                )
                .await
                .is_err());
            assert!(runtime_receiver.try_recv().is_err());
            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();

            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "10.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("already read".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(
                matches!(
                    delivered.first(),
                    Some(RuntimeEventKind::WorkspacePatch(patch))
                        if matches!(
                            patch.changes(),
                            [WorkspaceChange::ConversationUpsert(conversation)]
                                if conversation.last_read_ts() == Some("20.0")
                                    && conversation.local_read_ts() == Some("20.0")
                        )
                ),
                "the pending read patch must publish before the socket event"
            );
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(_),
                    attention: None,
                }
            )));
            assert!(!delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { .. }
            )));
            assert!(!delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::WorkspacePatch(patch)
                    if patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ConversationAttentionObserved { .. }
                    ))
            )));
            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                1
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
                0
            );
            assert_eq!(
                store
                    .load_history("D1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["10.0"]
            );
            let persisted_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "D1")
                .unwrap();
            assert_eq!(persisted_conversation.local_read_ts(), Some("20.0"));
            assert_eq!(persisted_conversation.unread_activity_count(), 0);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn socket_recovery_failure_skips_claim_and_queues_delta_behind_pending_read() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-pending-read-recovery-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("0.0"))]),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );

            store
                .install_conversation_batch_failure_trigger_for("D1")
                .await
                .unwrap();
            assert!(workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ReadAdvanced {
                        channel_id: "D1".into(),
                        ts: "20.0".into(),
                        remaining_unread: 0,
                    },
                )
                .await
                .is_err());
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                events,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "10.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("must not claim".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            let first = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("recovery-failed message did not reach compatibility UI")
                .expect("runtime event channel closed");
            assert!(matches!(
                first.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "10.0"
            ));
            assert!(runtime_receiver.try_recv().is_err());
            {
                let pending = workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned");
                assert_eq!(pending.len(), 2);
                let revisions = pending
                    .iter()
                    .map(|entry| entry.batch.as_ref().unwrap().revision())
                    .collect::<Vec<_>>();
                assert!(revisions[0] < revisions[1]);
            }
            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Failed),
                0
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                1
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
                0
            );
            assert_eq!(metrics.notification_claims, 0);

            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "30.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("new while recovery is blocked".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            let second = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("second recovery-failed message did not reach compatibility UI")
                .expect("runtime event channel closed");
            assert!(matches!(
                second.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "30.0"
            ));
            assert!(runtime_receiver.try_recv().is_err());
            assert_eq!(
                workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned")
                    .len(),
                3
            );
            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Failed),
                1
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                1
            );
            assert_eq!(metrics.notification_claims, 0);

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "40.0".into(),
                            user: Some("U_SELF".into()),
                            text: Some("recovery trigger".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let recovered_read = runtime_receiver.recv().await.unwrap();
            let recovered_stale_message = runtime_receiver.recv().await.unwrap();
            let recovered_new_message = runtime_receiver.recv().await.unwrap();
            let recovered_notification = runtime_receiver.recv().await.unwrap();
            let current_raw = runtime_receiver.recv().await.unwrap();
            let current_patch = runtime_receiver.recv().await.unwrap();
            let (
                RuntimeEventKind::WorkspacePatch(read_patch),
                RuntimeEventKind::WorkspacePatch(stale_message_patch),
                RuntimeEventKind::WorkspacePatch(new_message_patch),
            ) = (
                recovered_read.kind,
                recovered_stale_message.kind,
                recovered_new_message.kind,
            )
            else {
                panic!("older read and both message patches must recover first");
            };
            assert!(read_patch.revision() < stale_message_patch.revision());
            assert!(stale_message_patch.revision() < new_message_patch.revision());
            assert!(new_message_patch.changes().iter().any(|change| matches!(
                change,
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id,
                    observations,
                } if channel_id == "D1"
                    && observations.iter().any(|observation| observation.message_ts == "30.0")
            )));
            assert!(matches!(
                recovered_notification.kind,
                RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                    if message.ts == "30.0"
            ));
            assert!(matches!(
                current_raw.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "40.0"
            ));
            assert!(matches!(
                current_patch.kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(runtime_receiver.try_recv().is_err());

            let persisted_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "D1")
                .unwrap();
            assert_eq!(persisted_conversation.local_read_ts(), Some("20.0"));
            assert!(!persisted_conversation.has_observed_attention_message("10.0"));
            assert!(persisted_conversation.has_observed_attention_message("30.0"));
            assert_eq!(persisted_conversation.unread_activity_count(), 1);
            assert_eq!(
                store
                    .load_history("D1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["40.0", "30.0", "10.0"]
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn socket_and_local_post_authority_use_the_coordinator_journal() {
        let source = include_str!("runtime.rs");
        let post_command = source
            .split_once("RuntimeCommand::PostMessage {")
            .unwrap()
            .1
            .split_once("RuntimeCommand::SetReaction {")
            .unwrap()
            .0;
        assert!(post_command.contains("publish_local_post_message("));

        let local_persistence = source
            .split_once("async fn publish_local_post_message(")
            .unwrap()
            .1
            .split_once("async fn publish_local_thread_read(")
            .unwrap()
            .0;
        assert!(local_persistence.contains("publication_admission.lock().await"));
        assert!(local_persistence.contains("apply_and_enqueue("));
        assert!(local_persistence.contains("publish_posted_message_before_persistence("));
        assert!(!local_persistence.contains("store_merged_history("));
        assert!(!local_persistence.contains("store_merged_thread("));

        let socket_persistence = source
            .split_once("async fn persist_socket_message(")
            .unwrap()
            .1
            .split_once(
                "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\nstruct SocketModeReconnectTiming",
            )
            .unwrap()
            .0;
        assert!(socket_persistence.contains("publication_admission.lock().await"));
        assert!(socket_persistence.contains("recover_persisted_admitted(Some(store))"));
        assert!(socket_persistence.contains("apply_and_enqueue("));
        assert!(!socket_persistence.contains("store_merged_history("));
        assert!(!socket_persistence.contains("store_merged_thread("));
        assert!(!socket_persistence.contains("workspace.apply("));
        let recovered_patch_position = socket_persistence
            .find("publish_persisted_workspace_write")
            .unwrap();
        let raw_position = socket_persistence
            .rfind("events.send_event(RuntimeEventKind::SocketModeEvent")
            .unwrap();
        let patch_position = socket_persistence
            .rfind("publish_persisted_workspace_write")
            .unwrap();
        assert!(recovered_patch_position < raw_position);
        assert!(raw_position < patch_position);

        let worker_message_path = source
            .split_once("RealtimePersistenceEvent::Message { event } => {")
            .unwrap()
            .1
            .split_once("RealtimePersistenceEvent::OrderedEvent { event } =>")
            .unwrap()
            .0;
        assert!(worker_message_path.contains("persist_socket_message("));
        assert!(!worker_message_path.contains("workspace.apply("));
        assert!(!worker_message_path.contains("store_merged_history("));
        assert!(!worker_message_path.contains("store_merged_thread("));

        let queue_fallback = source
            .split_once("RealtimePersistenceQueueRejected reason=worker_closed")
            .unwrap()
            .1
            .split_once("workspace.trace_attention_metrics_snapshot()")
            .unwrap()
            .0;
        assert!(queue_fallback.contains(".schedule(returned.0)"));
        assert!(queue_fallback.contains("fallback.drain().await"));
        assert!(!queue_fallback.contains("workspace.apply("));
    }

    #[test]
    fn local_actions_have_no_store_first_or_nonjournaled_authority_bypasses() {
        let source = include_str!("runtime.rs");
        let conversation_read = source
            .split_once("RuntimeCommand::MarkConversationRead")
            .unwrap()
            .1
            .split_once("RuntimeCommand::MarkThreadRead")
            .unwrap()
            .0;
        assert!(conversation_read.contains("mark_conversation_read_best_effort("));
        assert!(!conversation_read.contains("clear_cached_conversation_unread("));
        assert!(!conversation_read.contains("apply_local_read_marker_compatibility("));
        let conversation_read_helper = source
            .split_once("async fn mark_conversation_read_best_effort(")
            .unwrap()
            .1
            .split_once("async fn publish_local_conversation_read(")
            .unwrap()
            .0;
        assert!(conversation_read_helper.contains("publish_local_conversation_read("));

        let thread_read = source
            .split_once("RuntimeCommand::MarkThreadRead")
            .unwrap()
            .1
            .split_once("RuntimeCommand::PostMessage")
            .unwrap()
            .0;
        assert!(thread_read.contains("publish_local_thread_read("));
        assert!(!thread_read.contains("store.mark_thread_read("));
        assert!(!thread_read.contains("load_cached_thread_catalog("));
        assert!(!thread_read.contains("context.workspace.apply("));

        let post = source
            .split_once("RuntimeCommand::PostMessage {")
            .unwrap()
            .1
            .split_once("RuntimeCommand::SetReaction")
            .unwrap()
            .0;
        assert!(post.contains("publish_local_post_message("));
        assert!(post.contains("backfill_requested_thread_ts("));
        assert!(!post.contains("persist_local_post_message("));
        assert!(!post.contains("context.workspace.apply("));
        assert!(!post.contains("store_merged_history("));
        assert!(!post.contains("store_merged_thread("));
    }

    #[test]
    fn local_post_completion_survives_store_failure_and_patches_recover_fifo() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-local-action-fifo-recovery-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let mut first = SlackConversation {
                id: "C1".into(),
                unread_count: Some(1),
                ..Default::default()
            };
            first.observe_attention_message_at("1.0", true);
            let second = SlackConversation {
                id: "C2".into(),
                ..Default::default()
            };
            seed_test_conversations(&store, &[first.clone(), second.clone()])
                .await
                .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![first, second],
                    ..Default::default()
                }),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::PostMessage, RuntimeTarget::Workspace),
            );

            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            publish_local_conversation_read(&events, Some(&store), &workspace, "C1", "2.0").await;
            let message = SlackMessage {
                ts: "3.0".into(),
                client_msg_id: Some("local-3".into()),
                user: Some("U_SELF".into()),
                text: Some("accepted while an older batch is unavailable".into()),
                ..Default::default()
            };
            publish_local_post_message(
                &events,
                Some(&store),
                &workspace,
                "C2".into(),
                message.clone(),
            )
            .await;

            let completion = receiver.recv().await.unwrap();
            assert!(matches!(
                completion.kind,
                RuntimeEventKind::MessagePosted { channel_id, message: posted }
                    if channel_id == "C2" && *posted == message
            ));
            assert!(receiver.try_recv().is_err());
            assert_eq!(
                workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned")
                    .len(),
                2
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            {
                let _admission = workspace.publication_admission.lock().await;
                persist_and_publish_local_reductions(
                    &events,
                    Some(&store),
                    &workspace,
                    "LocalActionRecovery",
                    "C1",
                )
                .await;
            }
            let recovered_read = receiver.recv().await.unwrap();
            let recovered_post = receiver.recv().await.unwrap();
            let revisions = [&recovered_read, &recovered_post]
                .into_iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch.revision().value(),
                    other => panic!("expected a recovered typed patch, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(revisions, vec![2, 3]);
            assert!(receiver.try_recv().is_err());
            assert!(workspace
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned")
                .is_empty());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let stored = load_test_conversations(&reopened).await.unwrap().unwrap();
            let stored_first = stored
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(stored_first.local_read_ts(), Some("2.0"));
            assert_eq!(stored_first.unread_activity_count(), 0);
            assert_eq!(
                reopened
                    .load_history("C2")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0"]
            );
            drop(reopened);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn local_post_and_realtime_echo_publish_one_typed_message_delta() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-local-post-echo-dedup-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::PostMessage, RuntimeTarget::Workspace),
            );
            let mut message = SlackMessage {
                ts: "1.0".into(),
                client_msg_id: Some("same-message".into()),
                user: Some("U_SELF".into()),
                text: Some("sent once".into()),
                ..Default::default()
            };
            backfill_requested_thread_ts(&mut message, Some("0.5"));
            assert_eq!(message.thread_ts.as_deref(), Some("0.5"));

            publish_local_post_message(
                &events,
                Some(&store),
                &workspace,
                "C1".into(),
                message.clone(),
            )
            .await;
            persist_socket_message(
                &store,
                Some("U_SELF"),
                &events,
                &workspace,
                crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: message.clone(),
                    kind: SocketModeMessageKind::Posted,
                },
            )
            .await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(event, RuntimeEventKind::WorkspacePatch(_)))
                    .count(),
                1
            );
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(event, RuntimeEventKind::MessagePosted { .. }))
                    .count(),
                1
            );
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(_),
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(workspace.history("C1").is_empty());
            assert!(store.load_history("C1").await.unwrap().is_none());
            let stored = store.load_thread("C1", "0.5").await.unwrap().unwrap();
            assert!(matches!(
                stored.as_slice(),
                [stored] if stored.ts == message.ts
                    && stored.thread_ts == message.thread_ts
                    && stored.client_msg_id == message.client_msg_id
            ));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn thread_read_batch_rolls_back_together_and_recovers_without_duplicate_patch() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-thread-read-atomic-recovery-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let mut conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            for ts in ["2.0", "3.0", "10.0"] {
                conversation.observe_attention_message_at(ts, true);
            }
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            let root = SlackMessage {
                ts: "1.0".into(),
                subscribed: Some(true),
                unread_count: Some(2),
                ..Default::default()
            };
            let replies = ["2.0", "3.0"]
                .into_iter()
                .map(|ts| SlackMessage {
                    ts: ts.into(),
                    thread_ts: Some("1.0".into()),
                    user: Some("U_OTHER".into()),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let mut thread = vec![root];
            thread.extend(replies);
            catalog.reconcile_complete_thread("C1", "1.0", &thread);
            let records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::ThreadCatalogReplaced(records.clone()),
                ],
            )
            .await
            .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation],
                    threads: records,
                    ..Default::default()
                }),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );

            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            publish_local_thread_read(&events, Some(&store), &workspace, "C1", "1.0", "3.0").await;
            if let Ok(event) = receiver.try_recv() {
                panic!("failed thread-read batch published {event:?}");
            }
            assert!(matches!(
                load_test_thread_catalog(&store).await.unwrap().as_slice(),
                [record] if record.unread == crate::thread_catalog::ThreadUnreadState::Known {
                    count: 2,
                    last_read: None,
                }
            ));
            assert_eq!(
                load_test_conversations(&store).await.unwrap().unwrap()[0].unread_activity_count(),
                3
            );

            store
                .clear_conversation_batch_failure_trigger()
                .await
                .unwrap();
            {
                let _admission = workspace.publication_admission.lock().await;
                persist_and_publish_local_reductions(
                    &events,
                    Some(&store),
                    &workspace,
                    "LocalThreadReadRecovery",
                    "C1",
                )
                .await;
            }
            let patch = receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(patch) = patch.kind else {
                panic!("thread read recovery must publish one workspace patch");
            };
            let records = patch
                .changes()
                .iter()
                .find_map(|change| match change {
                    WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                    _ => None,
                })
                .expect("thread read patch must contain its catalog projection");
            assert!(matches!(
                records.as_slice(),
                [record] if record.unread
                    == crate::thread_catalog::ThreadUnreadState::Known {
                        count: 0,
                        last_read: Some("3.0".to_string()),
                    }
            ));
            assert!(receiver.try_recv().is_err());

            let revision = workspace.revision();
            publish_local_thread_read(&events, Some(&store), &workspace, "C1", "1.0", "3.0").await;
            assert_eq!(workspace.revision(), revision);
            assert!(receiver.try_recv().is_err());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            assert!(matches!(
                load_test_thread_catalog(&reopened).await.unwrap().as_slice(),
                [record] if record.unread == crate::thread_catalog::ThreadUnreadState::Known {
                    count: 0,
                    last_read: Some("3.0".to_string()),
                }
            ));
            let stored = load_test_conversations(&reopened).await.unwrap().unwrap();
            assert_eq!(stored[0].unread_activity_count(), 1);
            assert!(stored[0].has_observed_attention_message("10.0"));
            drop(reopened);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn failed_socket_delta_sends_raw_then_recovers_patches_fifo() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-message-recovery-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            store
                .install_history_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            let self_post = |ts: &str, text: &str| RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: ts.into(),
                        user: Some("U_SELF".into()),
                        text: Some(text.into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            };
            sender.send(self_post("1.0", "first")).unwrap();

            let first = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("failed socket delta did not reach compatibility UI")
                .expect("runtime event channel closed");
            assert!(matches!(
                first.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "1.0"
            ));
            assert!(
                runtime_receiver.try_recv().is_err(),
                "failed current delta must not publish a typed patch"
            );
            assert_eq!(
                workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned")
                    .len(),
                1
            );

            store.clear_history_batch_failure_trigger().await.unwrap();
            sender.send(self_post("2.0", "second")).unwrap();
            drop(sender);
            worker.await.unwrap();

            let recovered = runtime_receiver.recv().await.unwrap();
            let second = runtime_receiver.recv().await.unwrap();
            let current = runtime_receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(recovered_patch) = recovered.kind else {
                panic!("older failed delta patch must recover first");
            };
            assert!(matches!(
                second.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    ..
                } if message.message.ts == "2.0"
            ));
            let RuntimeEventKind::WorkspacePatch(current_patch) = current.kind else {
                panic!("current patch must follow its compatibility event");
            };
            assert!(recovered_patch.revision() < current_patch.revision());
            assert!(runtime_receiver.try_recv().is_err());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            assert_eq!(
                reopened
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("2.0", "second".into()), ("1.0", "first".into())]
            );
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| (message.ts.as_str(), message.body_text()))
                    .collect::<Vec<_>>(),
                vec![("1.0", "first".into()), ("2.0", "second".into())]
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn realtime_attention_delta_projection_and_claim_recover_atomically() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-attention-atomic-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            store
                .install_history_batch_failure_trigger_for("D1")
                .await
                .unwrap();
            let transaction_baseline = store.committed_transaction_count().await.unwrap();

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let remote_message = crate::socket_mode::SocketModeMessageEvent {
                channel_id: "D1".into(),
                message: SlackMessage {
                    ts: "1.0".into(),
                    user: Some("U_OTHER".into()),
                    text: Some("persist me atomically".into()),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            };

            persist_socket_message(
                &store,
                Some("U_SELF"),
                &events,
                &workspace,
                remote_message.clone(),
            )
            .await;

            let raw = runtime_receiver.recv().await.unwrap();
            assert!(matches!(
                raw.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "1.0"
            ));
            assert!(
                runtime_receiver.try_recv().is_err(),
                "an uncommitted message must publish neither its typed patch nor notification"
            );
            assert_eq!(
                store.committed_transaction_count().await.unwrap(),
                transaction_baseline,
                "a failed atomic realtime batch must not commit a partial transaction"
            );
            assert!(store
                .load_history("D1")
                .await
                .unwrap()
                .unwrap_or_default()
                .is_empty());
            let failed_conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "D1")
                .unwrap();
            assert_eq!(failed_conversation.unread_activity_count(), 0);
            assert!(!failed_conversation.has_observed_attention_message("1.0"));

            store.clear_history_batch_failure_trigger().await.unwrap();
            persist_socket_message(
                &store,
                Some("U_SELF"),
                &events,
                &workspace,
                crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "2.0".into(),
                        user: Some("U_SELF".into()),
                        text: Some("recovery trigger".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                },
            )
            .await;

            let recovered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            let recovered_patch_position = recovered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::WorkspacePatch(patch)
                            if patch.changes().iter().any(|change| matches!(
                                change,
                                WorkspaceChange::TimelineChanged { changes, .. }
                                    if changes.iter().any(|change| matches!(
                                        change,
                                        crate::workspace_pipeline::MessageChange::Upsert(message)
                                            if message.ts == "1.0"
                                    ))
                            ))
                    )
                })
                .expect("the failed message patch must recover");
            let current_raw_position = recovered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(message),
                            ..
                        } if message.message.ts == "2.0"
                    )
                })
                .expect("the current raw event must be published");
            let current_patch_position = recovered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::WorkspacePatch(patch)
                            if patch.changes().iter().any(|change| matches!(
                                change,
                                WorkspaceChange::TimelineChanged { changes, .. }
                                    if changes.iter().any(|change| matches!(
                                        change,
                                        crate::workspace_pipeline::MessageChange::Upsert(message)
                                            if message.ts == "2.0"
                                    ))
                            ))
                    )
                })
                .expect("the current typed patch must be published");
            assert!(recovered_patch_position < current_raw_position);
            assert!(current_raw_position < current_patch_position);
            assert_eq!(
                recovered
                    .iter()
                    .filter(|event| matches!(
                        event,
                        RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                            if message.ts == "1.0"
                    ))
                    .count(),
                1,
                "the recovered atomic claim must notify exactly once"
            );
            assert_eq!(
                store.committed_transaction_count().await.unwrap() - transaction_baseline,
                2,
                "the recovered and current realtime messages must each commit one transaction"
            );

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversations = load_test_conversations(&reopened).await.unwrap().unwrap();
            let histories = HashMap::from([(
                "D1".into(),
                reopened
                    .load_history("D1")
                    .await
                    .unwrap()
                    .unwrap_or_default(),
            )]);
            let restarted_workspace = WorkspaceReducerAdapter::default();
            restarted_workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            restarted_workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations,
                    histories,
                    ..Default::default()
                }),
            );
            let restart_transaction_baseline =
                reopened.committed_transaction_count().await.unwrap();
            let (restart_events, mut restart_receiver) = mpsc::unbounded_channel();
            let restart_events = RuntimeEventSender::new(
                restart_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            persist_socket_message(
                &reopened,
                Some("U_SELF"),
                &restart_events,
                &restarted_workspace,
                remote_message,
            )
            .await;
            let duplicate = restart_receiver.recv().await.unwrap();
            assert!(matches!(
                duplicate.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "1.0"
            ));
            assert!(
                restart_receiver.try_recv().is_err(),
                "a restarted duplicate must not republish a patch or notification"
            );
            assert_eq!(
                reopened.committed_transaction_count().await.unwrap(),
                restart_transaction_baseline,
                "a duplicate with no remaining delta must not commit a transaction"
            );

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn closed_realtime_worker_persists_and_publishes_its_returned_last_event() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-closed-worker-fallback-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, persistence_receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            drop(persistence_receiver);
            let returned = sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("the last event must survive".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .expect_err("the closed worker must return ownership of the event")
                .0;
            let fallback = RealtimePersistenceFallback::new(
                Some(store.clone()),
                Some("U_SELF".into()),
                events,
                workspace.clone(),
                UserStatusSync::default(),
            );
            fallback.schedule(returned);
            fallback.drain().await;

            assert_eq!(
                store
                    .load_history("D1")
                    .await
                    .unwrap()
                    .unwrap_or_default()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                ["1.0"],
                "the returned last event must not require a later socket event to become durable"
            );
            assert!(!claim_test_attention_delivery(
                &store,
                workspace.revision().successor(),
                "D1",
                "1.0",
            )
            .await
            .unwrap());
            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            let raw = delivered
                .iter()
                .position(|event| matches!(event, RuntimeEventKind::SocketModeEvent { .. }))
                .expect("the returned event must publish its raw compatibility event");
            let patch = delivered
                .iter()
                .position(|event| matches!(event, RuntimeEventKind::WorkspacePatch(_)))
                .expect("the returned event must publish its typed patch");
            let notification = delivered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::AttentionNotificationCandidate { .. }
                    )
                })
                .expect("the returned event must publish its claimed notification");
            assert!(raw < patch && patch < notification);

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn no_store_message_fallback_publishes_raw_patch_and_notification() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("root".into()),
                ..Default::default()
            };
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation],
                    histories: HashMap::from([("D1".into(), vec![root])]),
                    ..Default::default()
                }),
            );
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let fallback = RealtimePersistenceFallback::new(
                None,
                Some("U_SELF".into()),
                events,
                workspace,
                UserStatusSync::default(),
            );
            fallback.schedule(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "2.0".into(),
                        thread_ts: Some("1.0".into()),
                        user: Some("U_OTHER".into()),
                        text: Some("reply without a cache store".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            });
            fallback.drain().await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            let raw = delivered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(message),
                            ..
                        } if message.message.ts == "2.0"
                    )
                })
                .expect("the no-store reply must publish its raw event");
            let patches = delivered
                .iter()
                .enumerate()
                .filter_map(|(position, event)| {
                    matches!(event, RuntimeEventKind::WorkspacePatch(_)).then_some(position)
                })
                .collect::<Vec<_>>();
            let notification = delivered
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                            if message.ts == "2.0"
                    )
                })
                .expect("the no-store DM reply must publish its notification");
            assert!(!patches.is_empty());
            assert!(raw < patches[0]);
            assert!(patches.last().unwrap() < &notification);
            assert!(matches!(
                delivered.get(patches[0]),
                Some(RuntimeEventKind::WorkspacePatch(patch))
                    if patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ThreadCatalogChanged(records)
                            if records.iter().any(|record| {
                                record.key.channel_id == "D1" && record.key.root_ts == "1.0"
                            })
                    ))
            ));
        });
    }

    #[test]
    fn unknown_realtime_conversation_persists_placeholder_attention_and_claim() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-unknown-attention-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );

            persist_socket_message(
                &store,
                Some("U_SELF"),
                &events,
                &workspace,
                crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D_UNKNOWN".into(),
                    message: SlackMessage {
                        ts: "1.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("unknown DM".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                },
            )
            .await;

            let persisted = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap_or_default()
                .into_iter()
                .find(|conversation| conversation.id == "D_UNKNOWN")
                .expect("accepted attention must persist its canonical placeholder");
            assert_eq!(persisted.unread_activity_count(), 1);
            assert!(persisted.has_observed_attention_message("1.0"));
            assert!(!claim_test_attention_delivery(
                &store,
                workspace.revision().successor(),
                "D_UNKNOWN",
                "1.0",
            )
            .await
            .unwrap());
            assert!(workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned")
                .conversation("D_UNKNOWN")
                .is_some_and(|conversation| {
                    conversation.has_observed_attention_message("1.0")
                }));
            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                    if message.ts == "1.0"
            )));

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn repair_claim_result_stays_with_original_pending_reduction() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-attention-repair-claim-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation.clone()),
            );
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store.corrupt_conversation_payload("D1").await.unwrap();
            store.validate_conversation_cache().await.unwrap();
            assert!(store.workspace_cache_needs_repair());
            let identity =
                crate::workspace_pipeline::AttentionDeliveryIdentity::new("D1", "1.0").unwrap();

            let reduction = workspace
                .apply_and_enqueue(
                    Some(&store),
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChangedWithDelivery {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("claimed by repair".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Realtime,
                        delivery: DeliveryState::Fresh,
                    },
                )
                .expect("fresh DM must produce a pending reduction");
            assert_eq!(
                reduction.store_batch().unwrap().notification_claims(),
                [identity.clone()]
            );

            let (repair_started, repair_started_receiver) = oneshot::channel();
            let (release_repair, repair_release_receiver) = oneshot::channel();
            workspace.set_workspace_repair_ack_gate(Arc::new(TestWorkspaceRepairAckGate {
                started: Mutex::new(Some(repair_started)),
                release: tokio::sync::Mutex::new(Some(repair_release_receiver)),
            }));
            let transaction_baseline = store.committed_transaction_count().await.unwrap();
            let repairing_workspace = workspace.clone();
            let repairing_store = store.clone();
            let repair = tokio::spawn(async move {
                let _admission = repairing_workspace.publication_admission.lock().await;
                repairing_workspace
                    .persist_pending_writes(Some(&repairing_store))
                    .await
            });
            repair_started_receiver.await.unwrap();
            workspace.apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "D1".into(),
                    is_im: Some(true),
                    name: Some("advanced during claim repair".into()),
                    ..Default::default()
                }),
            );
            release_repair.send(()).unwrap();
            repair.await.unwrap().unwrap();
            assert_eq!(
                store.committed_transaction_count().await.unwrap() - transaction_baseline,
                2,
                "each retry must atomically repair the projection and replay the claim"
            );
            assert!(
                workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned")
                    .iter()
                    .find(|entry| entry.reduction.is_some())
                    .unwrap()
                    .notification_claimed,
                "a duplicate retry result must not erase the first successful claim"
            );

            let persisted = workspace.drain_persisted_admitted();
            assert_eq!(persisted.len(), 1);
            assert!(persisted[0].notification_claimed);
            assert!(matches!(
                persisted[0].notification(),
                Some(MessageAttentionEffect { message, .. }) if message.ts == "1.0"
            ));
            assert!(!claim_test_attention_delivery(
                &store,
                workspace.revision().successor(),
                &identity.channel_id,
                &identity.message_ts,
            )
            .await
            .unwrap());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn socket_thread_catalog_raw_patch_and_notification_order_is_explicit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-event-order-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("root".into()),
                ..Default::default()
            };
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "D1".into(),
                        messages: vec![root.clone()],
                    },
                ],
            )
            .await
            .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation],
                    histories: HashMap::from([("D1".into(), vec![root])]),
                    ..Default::default()
                }),
            );

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store,
                Some("U_SELF".into()),
                event_sender,
                workspace,
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "2.0".into(),
                            thread_ts: Some("1.0".into()),
                            user: Some("U_OTHER".into()),
                            text: Some("reply".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(matches!(
                delivered.as_slice(),
                [
                    RuntimeEventKind::SocketModeEvent {
                        event: SocketModeEvent::Message(message),
                        attention: Some(_),
                    },
                    RuntimeEventKind::WorkspacePatch(patch),
                    RuntimeEventKind::AttentionNotificationCandidate { message: notified, .. },
                ] if message.message.ts == "2.0"
                    && notified.ts == "2.0"
                    && patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ConversationAttentionObserved { .. }
                    ))
                    && patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ThreadCatalogChanged(records) if !records.is_empty()
                    ))
            ));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn local_post_cache_and_closed_socket_fallback_preserve_canonical_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-local-post-cache-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            publish_local_post_message(
                &events,
                Some(&store),
                &workspace,
                "C1".into(),
                SlackMessage {
                    ts: "1.0".into(),
                    user: Some("U_SELF".into()),
                    text: Some("local channel post".into()),
                    ..Default::default()
                },
            )
            .await;
            publish_local_post_message(
                &events,
                Some(&store),
                &workspace,
                "C1".into(),
                SlackMessage {
                    ts: "2.0".into(),
                    thread_ts: Some("1.0".into()),
                    user: Some("U_SELF".into()),
                    text: Some("local reply".into()),
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["1.0"]
            );
            assert_eq!(
                store
                    .load_thread("C1", "1.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["2.0"]
            );

            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "D1".into(),
                    is_im: Some(true),
                    ..Default::default()
                }),
            );
            let fallback = crate::socket_mode::SocketModeMessageEvent {
                channel_id: "D1".into(),
                message: SlackMessage {
                    ts: "3.0".into(),
                    user: Some("U_OTHER".into()),
                    text: Some("queue fallback".into()),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            };
            let persistence_fallback = RealtimePersistenceFallback::new(
                Some(store.clone()),
                Some("U_SELF".into()),
                events.clone(),
                workspace.clone(),
                UserStatusSync::default(),
            );
            persistence_fallback.schedule(RealtimePersistenceEvent::Message {
                event: Box::new(fallback),
            });
            persistence_fallback.drain().await;
            assert_eq!(workspace.history("D1")[0].ts, "3.0");
            assert_eq!(
                workspace
                    .coordinator
                    .lock()
                    .expect("workspace coordinator lock poisoned")
                    .conversation("D1")
                    .unwrap()
                    .unread_activity_count(),
                1
            );
            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: Some(attention),
                } if message.message.ts == "3.0" && attention.record_unread
            )));
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                    if message.ts == "3.0"
            )));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn socket_thread_catalog_reconciles_reply_lifecycle_atomically_and_survives_restart() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-thread-catalog-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_ROOT".into()),
                text: Some("root".into()),
                reply_count: Some(3),
                latest_reply: Some("4.0".into()),
                reply_users: Some(vec!["U2".into(), "U3".into(), "U4".into()]),
                subscribed: Some(true),
                last_read: Some("1.0".into()),
                unread_count: Some(3),
                ..Default::default()
            };
            let reply = |ts: &str, user: &str, text: &str| SlackMessage {
                ts: ts.into(),
                thread_ts: Some("1.0".into()),
                user: Some(user.into()),
                text: Some(text.into()),
                ..Default::default()
            };
            let reply_two = reply("2.0", "U2", "reply two");
            let reply_three = reply("3.0", "U3", "reply three");
            let reply_four = reply("4.0", "U4", "reply four");
            let channel_five = SlackMessage {
                ts: "5.0".into(),
                user: Some("U5".into()),
                text: Some("channel five".into()),
                ..Default::default()
            };
            let thread = vec![
                root.clone(),
                reply_two.clone(),
                reply_three.clone(),
                reply_four.clone(),
            ];
            let history = vec![root.clone(), channel_five.clone()];
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread("C1", "1.0", &thread, true);
            let initial_records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: history.clone(),
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "1.0".into(),
                        messages: thread.clone(),
                    },
                    StoreChange::ThreadCatalogReplaced(initial_records.clone()),
                ],
            )
            .await
            .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation],
                    histories: HashMap::from([("C1".into(), history)]),
                    threads: initial_records,
                    ..Default::default()
                }),
            );
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        workspace.revision(),
                        crate::workspace_pipeline::MessagePage {
                            messages: thread,
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            );

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let edited_reply = SlackMessage {
                text: Some("reply two edited".into()),
                ..reply_two.clone()
            };
            let moved_into_thread = SlackMessage {
                thread_ts: Some("1.0".into()),
                ..channel_five.clone()
            };
            let broadcast_reply = SlackMessage {
                subtype: Some("thread_broadcast".into()),
                ..reply_four.clone()
            };
            let moved_out_of_thread = SlackMessage {
                thread_ts: None,
                subtype: None,
                ..broadcast_reply.clone()
            };
            run_realtime_message_events_for_test(
                store.clone(),
                workspace.clone(),
                events,
                vec![
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: edited_reply,
                        kind: SocketModeMessageKind::Changed,
                    },
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: reply_three,
                        kind: SocketModeMessageKind::Deleted,
                    },
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: moved_into_thread.clone(),
                        kind: SocketModeMessageKind::Changed,
                    },
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: moved_into_thread,
                        kind: SocketModeMessageKind::Changed,
                    },
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: broadcast_reply,
                        kind: SocketModeMessageKind::Changed,
                    },
                    crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: moved_out_of_thread,
                        kind: SocketModeMessageKind::Changed,
                    },
                ],
            )
            .await;

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(_),
                            ..
                        }
                    ))
                    .count(),
                6
            );
            assert!(!delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { .. }
            )));
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(event, RuntimeEventKind::WorkspacePatch(_)))
                    .count(),
                5,
                "the duplicate moved event must not create another typed patch"
            );

            let raw_positions = delivered
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(_),
                            ..
                        }
                    )
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            for (raw_ordinal, raw_position) in raw_positions.iter().copied().enumerate() {
                let next_raw = delivered
                    .iter()
                    .enumerate()
                    .skip(raw_position + 1)
                    .find_map(|(index, event)| {
                        matches!(
                            event,
                            RuntimeEventKind::SocketModeEvent {
                                event: SocketModeEvent::Message(_),
                                ..
                            }
                        )
                        .then_some(index)
                    })
                    .unwrap_or(delivered.len());
                let has_patch = delivered[raw_position + 1..next_raw]
                    .iter()
                    .any(|event| matches!(event, RuntimeEventKind::WorkspacePatch(_)));
                if raw_ordinal == 3 {
                    assert!(!has_patch, "the duplicate moved event must stay a no-op");
                } else {
                    assert!(
                        has_patch,
                        "each effective message delta must publish its typed patch after the raw event"
                    );
                }
            }

            let coordinator_records = delivered
                .iter()
                .filter_map(|event| match event {
                    RuntimeEventKind::WorkspacePatch(patch) => {
                        patch.changes().iter().find_map(|change| match change {
                            WorkspaceChange::ThreadCatalogChanged(records) => {
                                Some(records.clone())
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .last()
                .expect("coordinator did not publish its reconciled thread catalog");
            let record = coordinator_records
                .iter()
                .find(|record| {
                    record.key.channel_id == "C1" && record.key.root_ts == "1.0"
                })
                .unwrap();
            assert_eq!(record.reply_count, 2);
            assert_eq!(record.latest_reply.as_deref(), Some("5.0"));
            assert_eq!(
                record.unread,
                crate::thread_catalog::ThreadUnreadState::Known {
                    count: 2,
                    last_read: Some("1.0".into()),
                }
            );
            assert!(["U2", "U3", "U4", "U5"]
                .iter()
                .all(|user_id| record.participant_user_ids.contains(*user_id)));

            let coordinator_history = workspace.history("C1");
            let coordinator_root = coordinator_history
                .iter()
                .find(|message| message.ts == "1.0")
                .unwrap();
            assert_eq!(coordinator_root.reply_count, Some(2));
            assert_eq!(coordinator_root.latest_reply.as_deref(), Some("5.0"));
            assert!(coordinator_history.iter().any(|message| {
                message.ts == "4.0" && message.thread_ts.is_none() && message.subtype.is_none()
            }));
            assert!(!coordinator_history.iter().any(|message| message.ts == "5.0"));

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let restarted_records = load_test_thread_catalog(&reopened).await.unwrap();
            assert_eq!(restarted_records, coordinator_records);
            let restarted_thread = reopened.load_thread("C1", "1.0").await.unwrap().unwrap();
            assert_eq!(
                restarted_thread
                    .iter()
                    .filter(|message| message.thread_root_ts() == Some("1.0"))
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["5.0", "2.0"]
            );
            let restarted_root = restarted_thread
                .iter()
                .find(|message| message.ts == "1.0")
                .unwrap();
            assert_eq!(restarted_root.reply_count, Some(2));
            assert_eq!(restarted_root.latest_reply.as_deref(), Some("5.0"));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn duplicate_reply_delete_after_restart_keeps_catalog_and_roots_idempotent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-restarted-delete-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let mut root = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_ROOT".into()),
                text: Some("root".into()),
                reply_count: Some(2),
                latest_reply: Some("3.0".into()),
                reply_users: Some(vec!["U2".into(), "U3".into()]),
                subscribed: Some(true),
                last_read: Some("1.0".into()),
                unread_count: Some(2),
                ..Default::default()
            };
            let reply = |ts: &str, user: &str| SlackMessage {
                ts: ts.into(),
                thread_ts: Some("1.0".into()),
                user: Some(user.into()),
                text: Some(format!("reply {ts}")),
                ..Default::default()
            };
            let retained = reply("2.0", "U2");
            let deleted = reply("3.0", "U3");
            let initial_thread = vec![root.clone(), retained.clone(), deleted.clone()];
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread("C1", "1.0", &initial_thread, true);
            let initial_records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "1.0".into(),
                        messages: initial_thread.clone(),
                    },
                    StoreChange::ThreadCatalogReplaced(initial_records.clone()),
                ],
            )
            .await
            .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), vec![root.clone()])]),
                    threads: initial_records,
                    ..Default::default()
                }),
            );
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        workspace.revision(),
                        crate::workspace_pipeline::MessagePage {
                            messages: initial_thread,
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            );
            let (first_events, _first_receiver) = mpsc::unbounded_channel();
            run_realtime_message_events_for_test(
                store.clone(),
                workspace,
                RuntimeEventSender::new(
                    first_events,
                    RuntimeIdentity {
                        session: SessionId::default().next(),
                        request: RequestId::new(1),
                    },
                    OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
                ),
                vec![crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: deleted.clone(),
                    kind: SocketModeMessageKind::Deleted,
                }],
            )
            .await;

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let restarted_history = reopened.load_history("C1").await.unwrap().unwrap();
            let restarted_thread = reopened.load_thread("C1", "1.0").await.unwrap().unwrap();
            let restarted_records = load_test_thread_catalog(&reopened).await.unwrap();
            root = restarted_history
                .iter()
                .find(|message| message.ts == "1.0")
                .unwrap()
                .clone();
            assert_eq!(root.reply_count, Some(1));
            assert_eq!(root.latest_reply.as_deref(), Some("2.0"));
            let first_record = restarted_records
                .iter()
                .find(|record| record.key.root_ts == "1.0")
                .unwrap();
            assert_eq!(first_record.reply_count, 1);
            assert_eq!(
                first_record.unread,
                crate::thread_catalog::ThreadUnreadState::Known {
                    count: 1,
                    last_read: Some("1.0".into()),
                }
            );

            let restarted_workspace = WorkspaceReducerAdapter::default();
            restarted_workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            restarted_workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), restarted_history)]),
                    threads: restarted_records,
                    ..Default::default()
                }),
            );
            restarted_workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        restarted_workspace.revision(),
                        crate::workspace_pipeline::MessagePage {
                            messages: restarted_thread,
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            );
            let (duplicate_events, mut duplicate_receiver) = mpsc::unbounded_channel();
            run_realtime_message_events_for_test(
                reopened.clone(),
                restarted_workspace,
                RuntimeEventSender::new(
                    duplicate_events,
                    RuntimeIdentity {
                        session: SessionId::default().next(),
                        request: RequestId::new(2),
                    },
                    OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
                ),
                vec![crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: deleted,
                    kind: SocketModeMessageKind::Deleted,
                }],
            )
            .await;
            let duplicate_delivered = std::iter::from_fn(|| duplicate_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert_eq!(
                duplicate_delivered
                    .iter()
                    .filter(|event| matches!(
                        event,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Message(_),
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(!duplicate_delivered
                .iter()
                .any(|event| matches!(event, RuntimeEventKind::WorkspacePatch(_))));

            drop(reopened);
            let final_store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let final_record = load_test_thread_catalog(&final_store)
                .await
                .unwrap()
                .into_iter()
                .find(|record| record.key.root_ts == "1.0")
                .unwrap();
            assert_eq!(final_record.reply_count, 1);
            assert_eq!(
                final_record.unread,
                crate::thread_catalog::ThreadUnreadState::Known {
                    count: 1,
                    last_read: Some("1.0".into()),
                }
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn catalog_only_reply_move_persists_every_root_projection_across_restart() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-catalog-only-move-roots-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let mut first_root = SlackMessage {
                ts: "1.0".into(),
                text: Some("first root".into()),
                reply_count: Some(1),
                latest_reply: Some("2.0".into()),
                reply_users: Some(vec!["U2".into()]),
                ..Default::default()
            };
            let mut second_root = SlackMessage {
                ts: "10.0".into(),
                text: Some("second root".into()),
                reply_count: Some(0),
                reply_users: Some(Vec::new()),
                ..Default::default()
            };
            let previous = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                user: Some("U2".into()),
                text: Some("reply".into()),
                ..Default::default()
            };
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread("C1", "1.0", &[first_root.clone(), previous.clone()], true);
            catalog.observe_thread("C1", "10.0", std::slice::from_ref(&second_root), true);
            let records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![first_root.clone(), second_root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "1.0".into(),
                        messages: vec![first_root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                        messages: vec![second_root.clone()],
                    },
                    StoreChange::ThreadCatalogReplaced(records.clone()),
                ],
            )
            .await
            .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([(
                        "C1".into(),
                        vec![first_root.clone(), second_root.clone()],
                    )]),
                    threads: records,
                    ..Default::default()
                }),
            );
            let moved = SlackMessage {
                thread_ts: Some("10.0".into()),
                text: Some("moved reply".into()),
                ..previous
            };
            let (runtime_events, _runtime_receiver) = mpsc::unbounded_channel();
            run_realtime_message_events_for_test(
                store.clone(),
                workspace,
                RuntimeEventSender::new(
                    runtime_events,
                    RuntimeIdentity {
                        session: SessionId::default().next(),
                        request: RequestId::new(1),
                    },
                    OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
                ),
                vec![crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: moved,
                    kind: SocketModeMessageKind::Changed,
                }],
            )
            .await;

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            first_root = history
                .iter()
                .find(|message| message.ts == "1.0")
                .unwrap()
                .clone();
            second_root = history
                .iter()
                .find(|message| message.ts == "10.0")
                .unwrap()
                .clone();
            assert_eq!(first_root.reply_count, Some(0));
            assert_eq!(first_root.latest_reply, None);
            assert_eq!(second_root.reply_count, Some(1));
            assert_eq!(second_root.latest_reply.as_deref(), Some("2.0"));
            for (thread_ts, expected_count, expected_latest) in
                [("1.0", 0, None), ("10.0", 1, Some("2.0"))]
            {
                let thread = reopened
                    .load_thread("C1", thread_ts)
                    .await
                    .unwrap()
                    .unwrap();
                let root = thread
                    .iter()
                    .find(|message| message.ts == thread_ts)
                    .unwrap();
                assert_eq!(root.reply_count, Some(expected_count));
                assert_eq!(root.latest_reply.as_deref(), expected_latest);
            }
            let records = load_test_thread_catalog(&reopened).await.unwrap();
            let first_record = records
                .iter()
                .find(|record| record.key.root_ts == "1.0")
                .unwrap();
            let second_record = records
                .iter()
                .find(|record| record.key.root_ts == "10.0")
                .unwrap();
            assert_eq!(first_record.reply_count, 0);
            assert_eq!(second_record.reply_count, 1);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn repeated_stale_root_delete_after_move_preserves_clean_source_across_restart() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-stale-root-delete-after-move-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let first_root = SlackMessage {
                ts: "1.0".into(),
                reply_count: Some(2),
                latest_reply: Some("3.0".into()),
                reply_users: Some(vec!["U2".into(), "U3".into()]),
                ..Default::default()
            };
            let second_root = SlackMessage {
                ts: "10.0".into(),
                reply_count: Some(0),
                reply_users: Some(Vec::new()),
                ..Default::default()
            };
            let stale_location = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                user: Some("U2".into()),
                ..Default::default()
            };
            let retained = SlackMessage {
                ts: "3.0".into(),
                thread_ts: Some("1.0".into()),
                user: Some("U3".into()),
                ..Default::default()
            };
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread(
                "C1",
                "1.0",
                &[first_root.clone(), stale_location.clone(), retained.clone()],
                true,
            );
            catalog.observe_thread("C1", "10.0", std::slice::from_ref(&second_root), true);
            let records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![first_root.clone(), second_root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "1.0".into(),
                        messages: vec![
                            first_root.clone(),
                            stale_location.clone(),
                            retained.clone(),
                        ],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "10.0".into(),
                        messages: vec![second_root.clone()],
                    },
                    StoreChange::ThreadCatalogReplaced(records.clone()),
                ],
            )
            .await
            .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), vec![first_root, second_root])]),
                    threads: records,
                    ..Default::default()
                }),
            );
            let moved = SlackMessage {
                thread_ts: Some("10.0".into()),
                ..stale_location.clone()
            };
            for (request, message, kind) in [
                (1, moved, SocketModeMessageKind::Changed),
                (2, stale_location.clone(), SocketModeMessageKind::Deleted),
            ] {
                let (events, _receiver) = mpsc::unbounded_channel();
                run_realtime_message_events_for_test(
                    store.clone(),
                    workspace.clone(),
                    RuntimeEventSender::new(
                        events,
                        RuntimeIdentity {
                            session: SessionId::default().next(),
                            request: RequestId::new(request),
                        },
                        OperationContext::new(
                            RuntimeOperation::SocketMode,
                            RuntimeTarget::Workspace,
                        ),
                    ),
                    vec![crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message,
                        kind,
                    }],
                )
                .await;
            }

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let restarted_history = reopened.load_history("C1").await.unwrap().unwrap();
            let restarted_records = load_test_thread_catalog(&reopened).await.unwrap();
            let restarted_workspace = WorkspaceReducerAdapter::default();
            restarted_workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), restarted_history)]),
                    threads: restarted_records,
                    ..Default::default()
                }),
            );
            let (events, _receiver) = mpsc::unbounded_channel();
            run_realtime_message_events_for_test(
                reopened.clone(),
                restarted_workspace,
                RuntimeEventSender::new(
                    events,
                    RuntimeIdentity {
                        session: SessionId::default().next(),
                        request: RequestId::new(3),
                    },
                    OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
                ),
                vec![crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: stale_location,
                    kind: SocketModeMessageKind::Deleted,
                }],
            )
            .await;
            drop(reopened);

            let final_store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let history = final_store.load_history("C1").await.unwrap().unwrap();
            let first_root = history.iter().find(|message| message.ts == "1.0").unwrap();
            let second_root = history.iter().find(|message| message.ts == "10.0").unwrap();
            assert_eq!(first_root.reply_count, Some(1));
            assert_eq!(first_root.latest_reply.as_deref(), Some("3.0"));
            assert_eq!(second_root.reply_count, Some(0));
            assert_eq!(second_root.latest_reply, None);
            let records = load_test_thread_catalog(&final_store).await.unwrap();
            let first = records
                .iter()
                .find(|record| record.key.root_ts == "1.0")
                .unwrap();
            let second = records
                .iter()
                .find(|record| record.key.root_ts == "10.0")
                .unwrap();
            assert_eq!(first.reply_count, 1);
            assert_eq!(first.latest_reply.as_deref(), Some("3.0"));
            assert_eq!(second.reply_count, 0);
            assert_eq!(second.latest_reply, None);
            for (thread_ts, expected_count, expected_latest) in
                [("1.0", 1, Some("3.0")), ("10.0", 0, None)]
            {
                let thread = final_store
                    .load_thread("C1", thread_ts)
                    .await
                    .unwrap()
                    .unwrap();
                let root = thread
                    .iter()
                    .find(|message| message.ts == thread_ts)
                    .unwrap();
                assert_eq!(root.reply_count, Some(expected_count));
                assert_eq!(root.latest_reply.as_deref(), expected_latest);
            }
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn recovered_socket_thread_catalog_patch_precedes_next_raw_event() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-recovered-thread-catalog-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_ROOT".into()),
                text: Some("root".into()),
                reply_count: Some(0),
                subscribed: Some(true),
                unread_count: Some(0),
                ..Default::default()
            };
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread("C1", "1.0", std::slice::from_ref(&root), true);
            let initial_records = catalog.into_records();
            apply_test_store_changes(
                &store,
                vec![
                    StoreChange::ConversationsReplaced(vec![conversation.clone()]),
                    StoreChange::HistoryReplaced {
                        channel_id: "C1".into(),
                        messages: vec![root.clone()],
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C1".into(),
                        thread_ts: "1.0".into(),
                        messages: vec![root.clone()],
                    },
                    StoreChange::ThreadCatalogReplaced(initial_records.clone()),
                ],
            )
            .await
            .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation],
                    histories: HashMap::from([("C1".into(), vec![root.clone()])]),
                    threads: initial_records,
                    ..Default::default()
                }),
            );
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "1.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        workspace.revision(),
                        crate::workspace_pipeline::MessagePage {
                            messages: vec![root],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            );
            store
                .install_history_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                events,
                workspace,
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "2.0".into(),
                            thread_ts: Some("1.0".into()),
                            user: Some("U_SELF".into()),
                            text: Some("reply awaiting recovery".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();

            let failed_raw = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("failed message did not reach the raw UI path")
                .expect("runtime event channel closed");
            assert!(matches!(
                failed_raw.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "2.0"
            ));
            assert!(
                runtime_receiver.try_recv().is_err(),
                "an uncommitted catalog projection or typed patch escaped before recovery"
            );

            store.clear_history_batch_failure_trigger().await.unwrap();
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "3.0".into(),
                            user: Some("U_SELF".into()),
                            text: Some("next channel message".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let recovered_patch = runtime_receiver.recv().await.unwrap();
            let next_raw = runtime_receiver.recv().await.unwrap();
            let next_patch = runtime_receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(ref patch) = recovered_patch.kind else {
                panic!("recovered workspace patch must publish first");
            };
            let records = patch
                .changes()
                .iter()
                .find_map(|change| match change {
                    WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                    _ => None,
                })
                .expect("recovered patch did not include the thread catalog");
            let record = records
                .iter()
                .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "1.0")
                .unwrap();
            assert_eq!(record.reply_count, 1);
            assert_eq!(record.latest_reply.as_deref(), Some("2.0"));
            assert_eq!(
                record.unread,
                crate::thread_catalog::ThreadUnreadState::Known {
                    count: 0,
                    last_read: None,
                }
            );
            assert!(matches!(
                next_raw.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    ..
                } if message.message.ts == "3.0"
            ));
            assert!(matches!(
                next_patch.kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(runtime_receiver.try_recv().is_err());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            assert_eq!(
                load_test_thread_catalog(&reopened).await.unwrap(),
                records.clone()
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    async fn run_realtime_message_events_for_test(
        store: WorkspaceStore,
        workspace: WorkspaceReducerAdapter,
        events: RuntimeEventSender,
        messages: Vec<crate::socket_mode::SocketModeMessageEvent>,
    ) {
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store,
            Some("U_SELF".into()),
            events,
            workspace,
            UserStatusSync::default(),
        ));
        for event in messages {
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(event),
                })
                .unwrap();
        }
        drop(sender);
        worker.await.unwrap();
    }

    async fn assert_socket_history_admission_order(realtime_first: bool) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let order = if realtime_first {
            "realtime-first"
        } else {
            "history-first"
        };
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-history-{order}-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
        let events = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );

        let original_edit = SlackMessage {
            ts: "1.0".into(),
            client_msg_id: Some("edit".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale edit".into()),
            ..Default::default()
        };
        let original_deleted = SlackMessage {
            ts: "2.0".into(),
            client_msg_id: Some("delete".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale delete".into()),
            ..Default::default()
        };
        let original_moved = SlackMessage {
            ts: "3.0".into(),
            client_msg_id: Some("move".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale location".into()),
            ..Default::default()
        };
        let stale_history = vec![
            original_moved.clone(),
            original_deleted.clone(),
            original_edit.clone(),
        ];
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        ..Default::default()
                    }],
                    histories: HashMap::from([("C1".into(), stale_history.clone())]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            runtime_receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        let history_base = workspace.revision();

        let authoritative_edit = SlackMessage {
            text: Some("authoritative edit".into()),
            ..original_edit.clone()
        };
        let moved_to_thread = SlackMessage {
            thread_ts: Some("9.0".into()),
            text: Some("authoritative thread location".into()),
            ..original_moved.clone()
        };
        let concurrent_post = SlackMessage {
            ts: "4.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("concurrent post".into()),
            ..Default::default()
        };
        let socket_events = vec![
            crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: authoritative_edit,
                kind: SocketModeMessageKind::Changed,
            },
            crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: original_deleted,
                kind: SocketModeMessageKind::Deleted,
            },
            crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: moved_to_thread,
                kind: SocketModeMessageKind::Changed,
            },
            crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: concurrent_post,
                kind: SocketModeMessageKind::Posted,
            },
        ];
        let workspace_store = Some(store.clone());
        if realtime_first {
            run_realtime_message_events_for_test(
                store.clone(),
                workspace.clone(),
                events.clone(),
                socket_events.clone(),
            )
            .await;
        }
        publish_history_snapshot_with_completion(
            &events,
            &workspace_store,
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            history_base,
            stale_history,
            false,
            None,
            true,
            false,
            false,
        )
        .await
        .unwrap();
        if !realtime_first {
            run_realtime_message_events_for_test(
                store.clone(),
                workspace.clone(),
                events,
                socket_events,
            )
            .await;
        }

        let stored = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![
                ("4.0", "concurrent post".into()),
                ("1.0", "authoritative edit".into()),
            ]
        );
        assert_eq!(
            store
                .load_thread("C1", "9.0")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("3.0", "authoritative thread location".into())]
        );
        assert_eq!(
            workspace
                .history("C1")
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![
                ("1.0", "authoritative edit".into()),
                ("4.0", "concurrent post".into()),
            ]
        );
        let delivered_socket_kinds = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .filter_map(|event| match event.kind {
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    ..
                } => Some(message.kind),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delivered_socket_kinds,
            [
                SocketModeMessageKind::Changed,
                SocketModeMessageKind::Deleted,
                SocketModeMessageKind::Changed,
                SocketModeMessageKind::Posted,
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn socket_and_history_both_admission_orders_preserve_edit_delete_move_and_post() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");
        runtime.block_on(async {
            assert_socket_history_admission_order(false).await;
            assert_socket_history_admission_order(true).await;
        });
    }

    #[test]
    fn realtime_persistence_worker_drains_events_in_session_scope() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-persistence-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = crate::models::SlackConversation {
                id: "C1".into(),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let (runtime_events, _receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            for ts in ["1.0", "2.0"] {
                sender
                    .send(RealtimePersistenceEvent::Message {
                        event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                            channel_id: "C1".into(),
                            message: SlackMessage {
                                ts: ts.into(),
                                user: Some("U_OTHER".into()),
                                text: Some(format!("message {ts}")),
                                ..Default::default()
                            },
                            kind: SocketModeMessageKind::Posted,
                        }),
                    })
                    .unwrap();
            }
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "3.0".into(),
                            thread_ts: Some("1.0".into()),
                            user: Some("U_OTHER".into()),
                            text: Some("thread reply".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(history.len(), 2);
            let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
            assert_eq!(thread.len(), 1);
            assert_eq!(thread[0].ts, "3.0");
            let conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .remove(0);
            assert_eq!(conversation.unread_activity_count(), 3);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn realtime_persistence_preserves_socket_event_order() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-message-order-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            seed_test_conversations(
                &store,
                &[SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
            let (runtime_events, mut runtime_receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store,
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("message".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::OrderedEvent {
                    event: SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                        channel_id: "C1".into(),
                        ts: "1.0".into(),
                        name: "wave".into(),
                        user_id: "U_OTHER".into(),
                        added: true,
                    }),
                })
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::OrderedEvent {
                    event: SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                        channel_id: "C1".into(),
                        ts: "1.0".into(),
                        name: "wave".into(),
                        user_id: "U_OTHER".into(),
                        added: true,
                    }),
                })
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("message".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Deleted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            let raw_events = delivered
                .iter()
                .filter_map(|kind| match kind {
                    RuntimeEventKind::SocketModeEvent { event, .. } => Some(event),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(matches!(
                raw_events.as_slice(),
                [
                    SocketModeEvent::Message(posted),
                    SocketModeEvent::Reaction(_),
                    SocketModeEvent::Reaction(_),
                    SocketModeEvent::Message(deleted),
                ] if posted.kind == SocketModeMessageKind::Posted
                    && deleted.kind == SocketModeMessageKind::Deleted
            ));
            let reaction_raw_index = delivered
                .iter()
                .position(|kind| {
                    matches!(
                        kind,
                        RuntimeEventKind::SocketModeEvent {
                            event: SocketModeEvent::Reaction(_),
                            ..
                        }
                    )
                })
                .unwrap();
            let reaction_patch_indices = delivered
                .iter()
                .enumerate()
                .filter_map(|(index, kind)| match kind {
                    RuntimeEventKind::WorkspacePatch(patch)
                        if patch.changes().iter().any(workspace_change_has_wave) =>
                    {
                        Some(index)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(reaction_patch_indices.len(), 1);
            assert!(
                reaction_raw_index < reaction_patch_indices[0],
                "the legacy reaction event must retain raw-before-patch ordering"
            );
            let coordinator = workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            assert!(coordinator.history("C1").is_empty());
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn realtime_persistence_claims_each_notification_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-notification-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (sender, persistence_receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                persistence_receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            let event = crate::socket_mode::SocketModeMessageEvent {
                channel_id: "D1".into(),
                message: SlackMessage {
                    ts: "1.0".into(),
                    user: Some("U_OTHER".into()),
                    text: Some("hello".into()),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            };
            for _ in 0..2 {
                sender
                    .send(RealtimePersistenceEvent::Message {
                        event: Box::new(event.clone()),
                    })
                    .unwrap();
            }
            drop(sender);
            worker.await.unwrap();

            let mut notifications = 0;
            let mut attention_acceptance = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                match event.kind {
                    RuntimeEventKind::AttentionNotificationCandidate { .. } => {
                        notifications += 1;
                    }
                    RuntimeEventKind::SocketModeEvent {
                        event: SocketModeEvent::Message(_),
                        attention,
                    } => attention_acceptance.push(attention.is_some()),
                    _ => {}
                }
            }
            assert_eq!(notifications, 1);
            assert_eq!(attention_acceptance, [true, false]);
            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
                1
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AlreadyObserved),
                1
            );
            assert_eq!(metrics.notification_claims, 1);
            assert_eq!(metrics.queue_enqueued, 2);
            assert_eq!(metrics.queue_dequeued, 2);
            assert_eq!(metrics.queue_depth, 0);
            assert_eq!(metrics.queue_peak_depth, 2);
            let conversation = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(conversation.unread_activity_count(), 1);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn realtime_attention_persistence_failure_queues_delta_and_sends_raw_without_attention() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let invalid_directory = std::env::temp_dir().join(format!(
                "conduit-invalid-attention-store-{}-{nonce}",
                std::process::id()
            ));
            std::fs::write(&invalid_directory, b"not a directory").unwrap();
            let store = WorkspaceStore::new(invalid_directory.clone(), "T1:U_SELF");
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "D1".into(),
                    is_im: Some(true),
                    ..Default::default()
                }),
            );
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, persistence_receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                persistence_receiver,
                store,
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("deferred".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Failed),
                1
            );
            assert_eq!(metrics.committed_decisions, 1);
            assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1);
            assert_eq!(metrics.notification_claims, 0);
            assert_eq!(metrics.queue_enqueued, 1);
            assert_eq!(metrics.queue_dequeued, 1);
            assert_eq!(metrics.queue_depth, 0);
            assert_eq!(workspace.history("D1").len(), 1);
            assert_eq!(
                workspace
                    .coordinator
                    .lock()
                    .expect("workspace coordinator lock poisoned")
                    .conversation("D1")
                    .unwrap()
                    .unread_activity_count(),
                1
            );
            {
                let pending = workspace
                    .pending_writes
                    .lock()
                    .expect("pending workspace writes lock poisoned");
                assert_eq!(pending.len(), 1);
                let changes = pending[0].batch.as_ref().unwrap().changes();
                assert!(changes.iter().any(|change| matches!(
                    change,
                    StoreChange::MessageDelta {
                        channel_id,
                        message,
                        kind: MessageMutationKind::Posted,
                    } if channel_id == "D1" && message.ts == "1.0"
                )));
                assert!(changes.iter().any(|change| matches!(
                    change,
                    StoreChange::ConversationAttentionObserved {
                        channel_id,
                        observations,
                    } if channel_id == "D1"
                        && observations.iter().any(|observation| {
                            observation.message_ts == "1.0" && observation.record_unread
                        })
                )));
            }
            let event = receiver.recv().await.unwrap();
            assert!(matches!(
                event.kind,
                RuntimeEventKind::SocketModeEvent {
                    attention: None,
                    ..
                }
            ));
            assert!(
                receiver.try_recv().is_err(),
                "a failed MessageDelta must not publish its typed patch"
            );
            std::fs::remove_file(invalid_directory).unwrap();
        });
    }

    #[test]
    #[ignore = "release-mode realtime persistence measurement"]
    fn realtime_attention_persistence_burst_measurement() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-attention-burst-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversations = vec![
                SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                },
                SlackConversation {
                    id: "D1".into(),
                    is_im: Some(true),
                    ..Default::default()
                },
            ];
            seed_test_conversations(&store, &conversations)
                .await
                .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.update_attention_preferences(AttentionPreferences {
                keywords: vec!["priority phrase".into()],
                ..AttentionPreferences::default()
            });
            for conversation in conversations {
                workspace.apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::ConversationUpsert(conversation),
                );
            }
            let thread_root = SlackMessage {
                ts: "0.000001".into(),
                user: Some("U_OTHER".into()),
                text: Some("thread root".into()),
                reply_users: Some(vec!["U_SELF".into()]),
                ..Default::default()
            };
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        workspace.revision(),
                        crate::workspace_pipeline::MessagePage {
                            messages: vec![thread_root.clone()],
                            complete: false,
                            ..Default::default()
                        },
                    ),
                },
            );
            let baseline = workspace.attention_metrics_snapshot();

            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, persistence_receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let started = Instant::now();
            let worker = tokio::spawn(persist_realtime_events(
                persistence_receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            tokio::task::yield_now().await;
            let mut channel_messages = Vec::with_capacity(700);
            let mut direct_messages = Vec::with_capacity(200);
            let mut thread_messages = Vec::with_capacity(100);
            let mut duplicate_direct_messages = Vec::with_capacity(200);
            {
                let mut enqueue = |channel_id: &str,
                                   index: usize,
                                   text: &str,
                                   subtype: Option<&str>,
                                   thread_ts: Option<&str>| {
                    let message = SlackMessage {
                        ts: format!("{index}.000000"),
                        user: Some("U_OTHER".into()),
                        text: Some(text.into()),
                        subtype: subtype.map(str::to_string),
                        thread_ts: thread_ts.map(str::to_string),
                        ..Default::default()
                    };
                    if channel_id == "D1" {
                        direct_messages.push(message.clone());
                    } else if thread_ts.is_some() {
                        thread_messages.push(message.clone());
                    } else {
                        channel_messages.push(message.clone());
                    }
                    let event = crate::socket_mode::SocketModeMessageEvent {
                        channel_id: channel_id.into(),
                        message,
                        kind: SocketModeMessageKind::Posted,
                    };
                    if channel_id == "D1" {
                        duplicate_direct_messages.push(event.clone());
                    }
                    sender
                        .send(RealtimePersistenceEvent::Message {
                            event: Box::new(event),
                        })
                        .unwrap();
                };

                for index in 1..=400 {
                    enqueue("C1", index, "ordinary channel update", None, None);
                }
                for index in 401..=600 {
                    enqueue("D1", index, "direct conversation update", None, None);
                }
                for index in 601..=700 {
                    enqueue("C1", index, "explicit <@U_SELF> update", None, None);
                }
                for index in 701..=800 {
                    enqueue("C1", index, "configured priority phrase", None, None);
                }
                for index in 801..=900 {
                    enqueue(
                        "C1",
                        index,
                        "participated thread update",
                        None,
                        Some("0.000001"),
                    );
                }
                for index in 901..=1000 {
                    enqueue("C1", index, "membership update", Some("channel_join"), None);
                }
            }
            for event in duplicate_direct_messages {
                sender
                    .send(RealtimePersistenceEvent::Message {
                        event: Box::new(event),
                    })
                    .unwrap();
            }
            drop(sender);

            worker.await.unwrap();
            let drain_milliseconds = started.elapsed().as_millis();

            let metrics = workspace
                .attention_metrics_snapshot()
                .delta_since(&baseline);
            assert_eq!(metrics.committed_decisions, 1_000);
            assert_eq!(metrics.unread_decisions, 900);
            assert_eq!(metrics.notification_candidates, 500);
            assert_eq!(
                metrics.reason_count(AttentionReason::MembershipLifecycle),
                100
            );
            assert_eq!(metrics.origin_count(MutationOrigin::Realtime), 1_000);
            assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1_000);
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
                1_000
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AlreadyObserved),
                200
            );
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                0
            );
            assert_eq!(metrics.notification_claims, 500);
            assert_eq!(metrics.queue_enqueued, 1_200);
            assert_eq!(metrics.queue_dequeued, 1_200);
            assert_eq!(metrics.queue_depth, 0);
            let queue_peak = metrics.queue_peak_depth;
            assert!((1..=1_200).contains(&queue_peak));
            assert_eq!(metrics.queue_rejected, 0);

            let mut notification_events = 0;
            let mut message_events = 0;
            while let Ok(event) = receiver.try_recv() {
                match event.kind {
                    RuntimeEventKind::AttentionNotificationCandidate { .. } => {
                        notification_events += 1;
                    }
                    RuntimeEventKind::SocketModeEvent {
                        event: SocketModeEvent::Message(_),
                        ..
                    } => message_events += 1,
                    _ => {}
                }
            }
            assert_eq!(notification_events, 500);
            assert_eq!(message_events, 1_200);

            let before_reconciliation = workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned")
                .conversation("C1")
                .unwrap()
                .unread_activity_count();
            assert_eq!(before_reconciliation, 700);
            assert!(workspace
                .apply(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: SnapshotEnvelope::new(
                            workspace.revision(),
                            crate::workspace_pipeline::MessagePage {
                                messages: channel_messages,
                                complete: false,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .is_none());
            assert!(workspace
                .apply(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "D1".into(),
                        snapshot: SnapshotEnvelope::new(
                            workspace.revision(),
                            crate::workspace_pipeline::MessagePage {
                                messages: direct_messages,
                                complete: false,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .is_none());
            assert!(workspace
                .apply(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ThreadSnapshot {
                        channel_id: "C1".into(),
                        thread_ts: "0.000001".into(),
                        snapshot: SnapshotEnvelope::new(
                            workspace.revision(),
                            crate::workspace_pipeline::MessagePage {
                                messages: thread_messages,
                                complete: false,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .is_none());
            let raw_snapshot_base = workspace.revision();
            workspace.apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::UnreadChanged {
                    snapshot: SlackConversationUnreadSnapshot {
                        channel_id: "C1".into(),
                        unread_state: SlackUnreadState::from_parts(true, true, 9_999),
                        ..Default::default()
                    },
                    base_revision: raw_snapshot_base,
                },
            );
            {
                let coordinator = workspace
                    .coordinator
                    .lock()
                    .expect("workspace coordinator lock poisoned");
                let channel = coordinator.conversation("C1").unwrap();
                assert_eq!(channel.raw_unread_activity_count(), 9_999);
                assert_eq!(channel.unread_activity_count(), 700);
                assert_eq!(
                    coordinator
                        .conversation("D1")
                        .unwrap()
                        .unread_activity_count(),
                    200
                );
            }

            let reopened_store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let persisted = load_test_conversations(&reopened_store)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                persisted
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .unwrap()
                    .unread_activity_count(),
                700
            );
            assert_eq!(
                persisted
                    .iter()
                    .find(|conversation| conversation.id == "D1")
                    .unwrap()
                    .unread_activity_count(),
                200
            );

            eprintln!(
                "attention_realtime_burst ingress=1200 unique=1000 drain_ms={drain_milliseconds} \
                 queue_peak={queue_peak} persistence_accepted=1000 already_observed=200 unread=900 \
                 notification_claims=500"
            );
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn slack_read_cursor_classifies_realtime_attention_as_stale() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-slack-read-cursor-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                extra: HashMap::from([("last_read".into(), serde_json::json!("20.0"))]),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let transaction_baseline = store.committed_transaction_count().await.unwrap();

            persist_socket_message(
                &store,
                Some("U_SELF"),
                &events,
                &workspace,
                crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "10.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("already read by Slack".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                },
            )
            .await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Message(message),
                    attention: None,
                } if message.message.ts == "10.0"
            )));
            assert!(!delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { .. }
            )));
            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                1
            );
            assert_eq!(metrics.delivery_count(DeliveryState::Stale), 1);
            assert_eq!(
                store.committed_transaction_count().await.unwrap() - transaction_baseline,
                1
            );
            let persisted = load_test_conversations(&store)
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|conversation| conversation.id == "D1")
                .unwrap();
            assert_eq!(persisted.unread_activity_count(), 0);
            assert!(!persisted.has_observed_attention_message("10.0"));
            assert!(
                claim_test_attention_delivery(
                    &store,
                    workspace.revision().successor(),
                    "D1",
                    "10.0",
                )
                .await
                .unwrap(),
                "a stale message must not consume the notification claim identity"
            );

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn coordinator_read_rejection_keeps_timeline_without_restoring_attention() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-realtime-read-race-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            let mut conversation = SlackConversation {
                id: "C1".into(),
                extra: HashMap::from([("last_read".into(), serde_json::json!("20.0"))]),
                ..Default::default()
            };
            conversation.set_local_read_ts("20.0");
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let event_sender = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );
            let (sender, persistence_receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            let worker = tokio::spawn(persist_realtime_events(
                persistence_receiver,
                store,
                Some("U_SELF".into()),
                event_sender,
                workspace.clone(),
                UserStatusSync::default(),
            ));
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "10.0".into(),
                            user: Some("U_OTHER".into()),
                            text: Some("already read".into()),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let metrics = workspace.attention_metrics_snapshot();
            assert_eq!(
                metrics.persistence_count(AttentionPersistenceOutcome::AtOrBeforeReadCursor),
                1
            );
            assert_eq!(metrics.delivery_count(DeliveryState::Stale), 1);
            assert_eq!(metrics.reason_count(AttentionReason::StaleDelivery), 1);
            let event = receiver.recv().await.unwrap();
            assert!(matches!(
                event.kind,
                RuntimeEventKind::SocketModeEvent {
                    attention: None,
                    ..
                }
            ));
            let coordinator = workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            let conversation = coordinator.conversation("C1").unwrap();
            assert_eq!(conversation.unread_activity_count(), 0);
            assert!(!conversation.has_observed_attention_message("10.0"));
            assert_eq!(coordinator.history("C1").len(), 1);
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn replacing_session_aborts_registered_session_tasks() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();

            spawn_session_task(&state, first_session, async move {
                let _signal = CancellationSignal(Some(cancelled_tx));
                let _ = started_tx.send(());
                future::pending::<()>().await;
            });
            started_rx.await.expect("session task did not start");

            replace_runtime_session(&state, second_session).await;

            tokio::time::timeout(Duration::from_millis(100), cancelled_rx)
                .await
                .expect("old session task was not aborted")
                .expect("cancellation signal was dropped");
            assert_eq!(
                state
                    .lock()
                    .expect("runtime state lock poisoned")
                    .active_session,
                second_session
            );
        });
    }

    #[test]
    fn replacing_session_fences_a_superseded_task_before_the_old_store_barrier() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-session-replacement-task-fence-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U1");
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
            let (huddles, _huddle_receiver) = huddle_actor_channel();
            state.lock().unwrap().connection = Some(RuntimeConnection {
                slack: SlackApi::new(StoredToken {
                    access_token: "handoff-test-token".into(),
                    token_type: None,
                    scope: None,
                    refresh_token: None,
                    expires_in: None,
                    expires_at: None,
                    team_id: None,
                    team_name: None,
                    user_id: Some("U1".into()),
                    client_id: None,
                    browser_cookie_d: None,
                    user_agent: None,
                }),
                workspace_url: None,
                workspace_store: Some(store.clone()),
                workspace: WorkspaceReducerAdapter::default(),
                current_user_id: Some("U1".into()),
                user_cache: Arc::new(Mutex::new(HashMap::new())),
                read_marks: Arc::new(Mutex::new(HashMap::new())),
                message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                conversation_star_sync: ConversationStarSyncGate::default(),
                user_status_sync: UserStatusSync::default(),
                team_id: Some("T1".into()),
                huddles,
                cached_bootstrap_load_gate: None,
            });

            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (completed_tx, completed_rx) = oneshot::channel();
            let task_store = store.clone();
            spawn_request_task(
                &state,
                TrackedRequest::new(
                    RuntimeIdentity {
                        session: first_session,
                        request: RequestId::new(1),
                    },
                    OperationContext::new(RuntimeOperation::User, RuntimeTarget::Workspace),
                ),
                async move {
                    let _completion = CancellationSignal(Some(completed_tx));
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    drop(release_rx);
                    let _ = task_store
                        .store_custom_emojis(&HashMap::from([(
                            "late".to_string(),
                            "https://example.invalid/late.png".to_string(),
                        )]))
                        .await;
                },
            );
            tokio::task::spawn_blocking(move || {
                started_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("ordinary runtime task did not reach its pre-store gate");
            })
            .await
            .unwrap();
            state
                .lock()
                .unwrap()
                .cancel_active_work(ActiveWork::Task { task_id: 1 });

            let replacement = replace_runtime_session(&state, second_session);
            tokio::pin!(replacement);
            let replacement_while_task_is_running =
                tokio::time::timeout(Duration::from_millis(25), &mut replacement).await;
            assert_eq!(state.lock().unwrap().active_session, first_session);

            release_tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(1), completed_rx)
                .await
                .expect("aborted runtime task did not complete")
                .expect("runtime task completion signal was dropped");
            if replacement_while_task_is_running.is_err() {
                tokio::time::timeout(Duration::from_secs(1), replacement)
                    .await
                    .expect("session replacement did not finish after task completion");
            }

            assert!(
                replacement_while_task_is_running.is_err(),
                "session replacement passed the old store barrier before the aborted task completed"
            );
            assert_eq!(state.lock().unwrap().active_session, second_session);
            let transactions_after_replacement = store.committed_transaction_count().await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert_eq!(
                store.committed_transaction_count().await.unwrap(),
                transactions_after_replacement,
                "an old-session task wrote after the store handoff barrier"
            );

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn replacing_session_drains_scheduled_durable_work_before_reopening() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
            let scheduler = state
                .lock()
                .expect("runtime state lock poisoned")
                .sync_scheduler
                .clone();
            let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let gate = Arc::new(tokio::sync::Notify::new());
            let _ = scheduler
                .admit(
                    crate::sync_scheduler::SyncJob::new(
                        crate::sync_scheduler::SyncJobId::new(1),
                        crate::sync_scheduler::CancellationId::new(1),
                        crate::sync_scheduler::SyncTargetKey::new(
                            crate::sync_scheduler::SyncTargetKind::Workspace,
                            1,
                        ),
                        crate::sync_scheduler::SyncPriority::Interactive,
                        crate::sync_scheduler::SyncDurability::DurableAction,
                        crate::sync_scheduler::FreshnessPolicy::Always,
                        crate::sync_scheduler::ReplacementClass::Never,
                        crate::sync_scheduler::RetryPolicy::Never,
                    )
                    .unwrap(),
                    None,
                    crate::runtime_sync::RuntimeSyncWork::new({
                        let started = Arc::clone(&started);
                        let gate = Arc::clone(&gate);
                        move |_| {
                            let started = Arc::clone(&started);
                            let gate = Arc::clone(&gate);
                            async move {
                                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                gate.notified().await;
                                crate::sync_scheduler::JobOutcome::Succeeded
                            }
                        }
                    }),
                )
                .unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while started.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("durable scheduled work did not start");

            let replacement = replace_runtime_session(&state, second_session);
            tokio::pin!(replacement);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut replacement)
                    .await
                    .is_err(),
                "session replacement must drain accepted durable sync work"
            );
            gate.notify_one();
            tokio::time::timeout(Duration::from_secs(1), replacement)
                .await
                .expect("session replacement did not drain durable sync work");

            assert_eq!(
                scheduler.shutdown_phase(),
                crate::sync_scheduler::ShutdownPhase::Drained
            );
            let state = state.lock().expect("runtime state lock poisoned");
            assert_eq!(state.active_session, second_session);
            assert_eq!(
                state.sync_scheduler.shutdown_phase(),
                crate::sync_scheduler::ShutdownPhase::Open
            );
        });
    }

    #[test]
    fn replacing_session_drains_an_accepted_socket_fallback_before_retiring_it() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-session-replacement-socket-drain-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let conversation = SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            };
            seed_test_conversations(&store, std::slice::from_ref(&conversation))
                .await
                .unwrap();
            let transaction_baseline = store.committed_transaction_count().await.unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
            let admission = workspace.publication_admission.lock().await;
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
            let (runtime_events, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                runtime_events,
                RuntimeIdentity {
                    session: first_session,
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            )
            .unsolicited(OperationContext::new(
                RuntimeOperation::SocketMode,
                RuntimeTarget::Workspace,
            ));
            let (accepted_tx, accepted_rx) = oneshot::channel();
            let fallback_store = store.clone();
            let fallback_workspace = workspace.clone();

            spawn_socket_mode_supervisor(
                &state,
                first_session,
                move |mut cancellation| async move {
                    let fallback = RealtimePersistenceFallback::new(
                        Some(fallback_store),
                        Some("U_SELF".into()),
                        events,
                        fallback_workspace,
                        UserStatusSync::default(),
                    );
                    fallback.schedule(RealtimePersistenceEvent::Message {
                        event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                            channel_id: "D1".into(),
                            message: SlackMessage {
                                ts: "1.0".into(),
                                user: Some("U_OTHER".into()),
                                text: Some("accepted before replacement".into()),
                                ..Default::default()
                            },
                            kind: SocketModeMessageKind::Posted,
                        }),
                    });
                    let _ = accepted_tx.send(());
                    while !*cancellation.borrow() {
                        if cancellation.changed().await.is_err() {
                            return;
                        }
                    }
                    fallback.drain().await;
                },
            );
            accepted_rx
                .await
                .expect("the socket supervisor did not accept its final event");

            let replacement = replace_runtime_session(&state, second_session);
            tokio::pin!(replacement);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut replacement)
                    .await
                    .is_err(),
                "session replacement must wait for accepted socket persistence"
            );
            drop(admission);
            tokio::time::timeout(Duration::from_secs(2), replacement)
                .await
                .expect("session replacement did not drain the socket supervisor");

            assert_eq!(
                state
                    .lock()
                    .expect("runtime state lock poisoned")
                    .active_session,
                second_session
            );
            assert_eq!(
                store
                    .load_history("D1")
                    .await
                    .unwrap()
                    .unwrap_or_default()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                ["1.0"]
            );
            let transactions_after_replacement = store.committed_transaction_count().await.unwrap();
            assert_eq!(
                transactions_after_replacement - transaction_baseline,
                1,
                "the accepted final event should settle in one store transaction"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert_eq!(
                store.committed_transaction_count().await.unwrap(),
                transactions_after_replacement,
                "the retired supervisor must leave no detached store writer"
            );
            assert!(
                !claim_test_attention_delivery(
                    &store,
                    workspace.revision().successor(),
                    "D1",
                    "1.0",
                )
                .await
                .unwrap(),
                "the accepted notification claim must be durable before replacement completes"
            );
            let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                delivered
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        RuntimeEventKind::AttentionNotificationCandidate { .. }
                    ))
                    .count(),
                1,
                "a durable claim must have exactly one matching UI candidate"
            );
            assert!(delivered
                .iter()
                .all(|event| event.meta.session == first_session));
            assert!(
                receiver.try_recv().is_err(),
                "the retired socket supervisor must not publish detached events"
            );
            assert!(state
                .lock()
                .expect("runtime state lock poisoned")
                .socket_mode_supervisor
                .is_none());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn latest_attention_preferences_seed_new_sessions_without_a_connection() {
        let first_session = SessionId::default().next();
        let second_session = first_session.next();
        let mut state = RuntimeState::new(first_session);
        let first = AttentionPreferences {
            direct_messages: false,
            ..AttentionPreferences::default()
        };
        let latest = AttentionPreferences {
            desktop_notifications: false,
            direct_messages: false,
            names_and_aliases: vec!["Vincent".to_string()],
            keywords: vec!["incident review".to_string()],
            ..AttentionPreferences::default()
        };

        state.set_attention_preferences(first);
        state.set_attention_preferences(latest.clone());
        assert_eq!(state.attention_preferences, latest);

        assert!(state.begin_session_replacement().is_none());
        state.finish_session_replacement(second_session);
        let seeded = state.attention_context(Some("U_NEXT".to_string()));
        assert_eq!(seeded.current_user_id.as_deref(), Some("U_NEXT"));
        assert_eq!(state.attention_preferences, latest);
    }

    #[test]
    fn connected_attention_update_changes_runtime_and_coordinator_together() {
        let session = SessionId::default().next();
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "D1".into(),
                is_channel: Some(false),
                is_im: Some(true),
                ..Default::default()
            }),
        );
        let (huddles, _huddle_receiver) = huddle_actor_channel();
        let mut state = RuntimeState::new(session);
        state.connection = Some(RuntimeConnection {
            slack: SlackApi::new(StoredToken {
                access_token: "test-token".into(),
                token_type: None,
                scope: None,
                refresh_token: None,
                expires_in: None,
                expires_at: None,
                team_id: None,
                team_name: None,
                user_id: Some("U_SELF".into()),
                client_id: None,
                browser_cookie_d: None,
                user_agent: None,
            }),
            workspace_url: None,
            workspace_store: None,
            workspace: workspace.clone(),
            current_user_id: Some("U_SELF".into()),
            user_cache: Arc::new(Mutex::new(HashMap::new())),
            read_marks: Arc::new(Mutex::new(HashMap::new())),
            message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(256))),
            conversation_star_sync: ConversationStarSyncGate::default(),
            user_status_sync: UserStatusSync::default(),
            team_id: None,
            huddles,
            cached_bootstrap_load_gate: None,
        });
        let disabled = AttentionPreferences {
            direct_messages: false,
            ..AttentionPreferences::default()
        };

        state.set_attention_preferences(disabled.clone());

        assert_eq!(state.attention_preferences, disabled);
        let effect = workspace
            .preview_message_attention(
                "D1",
                &SlackMessage {
                    ts: "1.0".into(),
                    user: Some("U_OTHER".into()),
                    text: Some("hello".into()),
                    ..Default::default()
                },
                MessageMutationKind::Posted,
                MutationOrigin::Realtime,
            )
            .expect("direct message should still be classified");
        assert!(effect.decision.record_unread);
        assert!(!effect.decision.send_notification);
    }

    #[test]
    fn image_asset_cache_key_is_stable_hex_digest() {
        assert_eq!(
            image_asset_cache_key("https://files.example/image.png"),
            "7db09e79cb28f1be72da3c1449cd42619e048f148310325cc2c8f55cd713aa0e"
        );
    }

    #[test]
    fn attachment_cache_path_is_stable_and_sanitizes_remote_filename() {
        let path = attachment_cache_path(
            "https://files.slack.com/files-pri/F1/download/report",
            "../../Quarterly: report?.pdf",
        );
        let filename = path.file_name().and_then(|name| name.to_str()).unwrap();

        assert!(path.starts_with(config::attachment_cache_dir()));
        assert!(filename.ends_with("-Quarterly_ report_.pdf"));
        assert!(!filename.contains('/'));
        assert!(!filename.contains(".."));
        assert_eq!(
            path,
            attachment_cache_path(
                "https://files.slack.com/files-pri/F1/download/report",
                "../../Quarterly: report?.pdf",
            )
        );
    }

    #[test]
    fn attachment_cache_filename_stays_within_a_byte_safe_component_limit() {
        let name = format!("{}.pdf", "é".repeat(200));
        let path = attachment_cache_path("https://files.slack.com/long-name", &name);
        let filename = path.file_name().and_then(|name| name.to_str()).unwrap();
        let basename = filename.split_once('-').unwrap().1;

        assert!(basename.len() <= ATTACHMENT_BASENAME_MAX_BYTES);
        assert!(filename.len() <= 64 + 1 + ATTACHMENT_BASENAME_MAX_BYTES);
        assert!(basename.is_char_boundary(basename.len()));
    }

    #[test]
    fn attachment_cache_prunes_expired_files_but_preserves_active_download() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-attachment-age-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let expired = directory.join("expired");
        let protected = directory.join("protected");
        std::fs::write(&expired, b"old").unwrap();
        std::fs::write(&protected, b"active").unwrap();

        prune_attachment_cache(
            &directory,
            Some(&protected),
            AttachmentCachePolicy {
                max_age: Duration::from_secs(5),
                max_bytes: u64::MAX,
            },
            SystemTime::now() + Duration::from_secs(10),
        )
        .unwrap();

        assert!(!expired.exists());
        assert!(protected.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attachment_cache_evicts_to_size_cap_without_removing_active_download() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-attachment-size-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let protected = directory.join("protected");
        let partial = directory.join("concurrent.123.part");
        std::fs::write(directory.join("first"), b"1111").unwrap();
        std::fs::write(directory.join("second"), b"2222").unwrap();
        std::fs::write(&protected, b"3333").unwrap();
        std::fs::write(&partial, b"download in progress").unwrap();

        prune_attachment_cache(
            &directory,
            Some(&protected),
            AttachmentCachePolicy {
                max_age: Duration::MAX,
                max_bytes: 7,
            },
            SystemTime::now(),
        )
        .unwrap();

        let retained_size = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "part")
            })
            .map(|entry| entry.metadata().unwrap().len())
            .sum::<u64>();
        assert!(protected.exists());
        assert!(partial.exists());
        assert!(retained_size <= 7);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn temporary_upload_guard_removes_staged_file() {
        let path = std::env::temp_dir().join(format!(
            "conduit-upload-cleanup-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::write(&path, b"screenshot").unwrap();

        {
            let _guard = RemoveFileOnDrop::new(true, &path);
            assert!(path.exists());
        }

        assert!(!path.exists());
    }

    #[test]
    fn workspace_store_id_uses_team_and_user_identity() {
        let auth = AuthInfo {
            team: Some("Example".to_string()),
            team_id: Some("T123".to_string()),
            user_id: Some("U123".to_string()),
            ..Default::default()
        };

        assert_eq!(workspace_store_id(&auth), "T123:U123");
    }

    #[test]
    fn runtime_command_context_identifies_operation_and_target() {
        assert_eq!(
            RuntimeCommand::SearchMessages {
                query: "from:ada".to_string(),
            }
            .operation_context(),
            OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace)
        );
        assert_eq!(
            RuntimeCommand::LoadThread {
                channel_id: "C123".to_string(),
                ts: "1710000000.000001".to_string(),
            }
            .operation_context(),
            OperationContext::new(
                RuntimeOperation::Thread,
                RuntimeTarget::Thread {
                    channel_id: "C123".to_string(),
                    thread_ts: "1710000000.000001".to_string(),
                },
            )
        );
        assert_eq!(
            RuntimeCommand::DiscoverConversations.operation_context(),
            OperationContext::new(
                RuntimeOperation::ConversationDiscovery,
                RuntimeTarget::Workspace,
            )
        );
        assert_eq!(
            RuntimeCommand::OpenDirectMessage {
                user_id: "U123".to_string(),
            }
            .operation_context(),
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::User("U123".to_string()),
            )
        );
        assert_eq!(
            RuntimeCommand::LoadFile {
                file_id: "F123".to_string(),
                share_requested: false,
            }
            .operation_context(),
            OperationContext::new(
                RuntimeOperation::Files,
                RuntimeTarget::File("F123".to_string()),
            )
        );
        assert_eq!(
            RuntimeCommand::Huddle(crate::huddles::state::HuddleCommand::OpenPreflight {
                call_id: "R123".to_string(),
            })
            .operation_context(),
            OperationContext::new(
                RuntimeOperation::Huddle,
                RuntimeTarget::Huddle("R123".to_string()),
            )
        );
        assert_eq!(
            RuntimeCommand::ResolveMessagePermalink {
                channel_id: "C123".to_string(),
                ts: "1710000000.000100".to_string(),
            }
            .operation_context(),
            OperationContext::new(
                RuntimeOperation::MessagePermalink,
                RuntimeTarget::ExactMessage {
                    channel_id: "C123".to_string(),
                    ts: "1710000000.000100".to_string(),
                },
            )
        );

        let channel_context = RuntimeCommand::LoadMessageContext(
            SearchMessageLocation::new("C123", "1710000000.000100", None).unwrap(),
        );
        assert_eq!(
            channel_context.operation_context(),
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C123".to_string()),
            )
        );
        assert_eq!(
            channel_context.navigation_slot(),
            Some(NavigationSlot::Main)
        );

        let thread_context = RuntimeCommand::LoadMessageContext(
            SearchMessageLocation::new("C123", "1710000001.000100", Some("1710000000.000100"))
                .unwrap(),
        );
        assert_eq!(
            thread_context.operation_context(),
            OperationContext::new(
                RuntimeOperation::Thread,
                RuntimeTarget::Thread {
                    channel_id: "C123".to_string(),
                    thread_ts: "1710000000.000100".to_string(),
                },
            )
        );
        assert_eq!(
            thread_context.navigation_slot(),
            Some(NavigationSlot::Thread)
        );
    }

    #[test]
    fn runtime_command_descriptor_owns_scheduling_policy() {
        let main_navigation = RuntimeCommand::LoadFiles.descriptor();
        assert_eq!(main_navigation.lane, RuntimeTaskLane::Navigation);
        assert_eq!(main_navigation.navigation_slot, Some(NavigationSlot::Main));
        assert!(main_navigation.supersedes_previous);

        let file_navigation = RuntimeCommand::LoadFile {
            file_id: "F123".to_string(),
            share_requested: false,
        }
        .descriptor();
        assert_eq!(file_navigation.lane, RuntimeTaskLane::Navigation);
        assert_eq!(file_navigation.navigation_slot, Some(NavigationSlot::Main));

        let background = RuntimeCommand::DiscoverConversations.descriptor();
        assert_eq!(background.lane, RuntimeTaskLane::Background);
        assert_eq!(background.navigation_slot, None);

        let channel_discovery = RuntimeCommand::DiscoverChannels.descriptor();
        assert_eq!(channel_discovery.lane, RuntimeTaskLane::Background);
        assert_eq!(
            channel_discovery.context,
            OperationContext::new(
                RuntimeOperation::ConversationDiscovery,
                RuntimeTarget::Workspace,
            )
        );

        let image = RuntimeCommand::LoadImageAsset {
            key: "preview".to_string(),
            url: "https://files.slack.com/preview".to_string(),
        }
        .descriptor();
        assert_eq!(image.lane, RuntimeTaskLane::Image);

        let upload = RuntimeCommand::UploadFile {
            channel_id: "C123".to_string(),
            thread_ts: None,
            path: PathBuf::from("upload.png"),
            initial_comment: None,
            remove_after_upload: false,
        }
        .descriptor();
        assert_eq!(upload.lane, RuntimeTaskLane::Upload);
        assert!(!upload.supersedes_previous);

        let interactive = RuntimeCommand::MarkConversationRead {
            channel_id: "C123".to_string(),
            ts: "1710000000.000100".to_string(),
        }
        .descriptor();
        assert_eq!(interactive.lane, RuntimeTaskLane::Interactive);
        assert!(interactive.supersedes_previous);

        let permalink = RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C123".to_string(),
            ts: "1710000000.000100".to_string(),
        }
        .descriptor();
        assert_eq!(permalink.lane, RuntimeTaskLane::Interactive);
        assert!(permalink.supersedes_previous);

        let leave = RuntimeCommand::LeaveConversation {
            channel_id: "C123".to_string(),
        }
        .descriptor();
        assert_eq!(leave.lane, RuntimeTaskLane::Interactive);
        assert!(!leave.supersedes_previous);
        assert_eq!(
            leave.context,
            OperationContext::new(
                RuntimeOperation::LeaveConversation,
                RuntimeTarget::Channel("C123".to_string()),
            )
        );

        let huddle = RuntimeCommand::Huddle(crate::huddles::state::HuddleCommand::SetMuted(true))
            .descriptor();
        assert_eq!(huddle.lane, RuntimeTaskLane::Interactive);
        assert!(!huddle.supersedes_previous);
        assert_eq!(
            huddle.context,
            OperationContext::new(
                RuntimeOperation::Huddle,
                RuntimeTarget::Huddle("active".to_string()),
            )
        );
    }

    #[test]
    fn bounded_sync_command_plans_cover_reads_without_carrying_payload_content() {
        let main_location = SearchMessageLocation::new("C1", "1710000001.000100", None).unwrap();
        let thread_location =
            SearchMessageLocation::new("C1", "1710000002.000100", Some("1710000000.000100"))
                .unwrap();
        let scheduled = [
            RuntimeCommand::RefreshConversations,
            RuntimeCommand::DiscoverChannels,
            RuntimeCommand::DiscoverConversations,
            RuntimeCommand::LoadHistory {
                channel_id: "C1".to_string(),
            },
            RuntimeCommand::LoadOlderHistory {
                channel_id: "C1".to_string(),
                cursor: "cursor-a".to_string(),
            },
            RuntimeCommand::LoadThread {
                channel_id: "C1".to_string(),
                ts: "1710000000.000100".to_string(),
            },
            RuntimeCommand::LoadOlderThread {
                channel_id: "C1".to_string(),
                ts: "1710000000.000100".to_string(),
                cursor: "cursor-b".to_string(),
            },
            RuntimeCommand::LoadMessageContext(main_location),
            RuntimeCommand::LoadMessageContext(thread_location),
            RuntimeCommand::SearchMessages {
                query: "first query".to_string(),
            },
            RuntimeCommand::LoadFiles,
            RuntimeCommand::LoadFile {
                file_id: "F1".to_string(),
                share_requested: false,
            },
            RuntimeCommand::LoadSavedItems,
        ];
        assert!(scheduled
            .iter()
            .all(|command| connected_command_sync_plan(command).is_some()));

        let excluded = [
            RuntimeCommand::JoinConversation {
                channel_id: "C1".to_string(),
            },
            RuntimeCommand::LoadUserProfile {
                user_id: "U1".to_string(),
            },
            RuntimeCommand::PostMessage {
                channel_id: "C1".to_string(),
                text: "message text".to_string(),
                thread_ts: None,
            },
            RuntimeCommand::LoadImageAsset {
                key: "preview-key".to_string(),
                url: "https://files.example.invalid/private".to_string(),
            },
        ];
        assert!(excluded
            .iter()
            .all(|command| connected_command_sync_plan(command).is_none()));

        let first_search = connected_command_sync_plan(&RuntimeCommand::SearchMessages {
            query: "first query".to_string(),
        })
        .unwrap();
        let second_search = connected_command_sync_plan(&RuntimeCommand::SearchMessages {
            query: "different query".to_string(),
        })
        .unwrap();
        assert_eq!(first_search.target, second_search.target);

        let first_page = connected_command_sync_plan(&RuntimeCommand::LoadOlderHistory {
            channel_id: "C1".to_string(),
            cursor: "cursor-a".to_string(),
        })
        .unwrap();
        let second_page = connected_command_sync_plan(&RuntimeCommand::LoadOlderHistory {
            channel_id: "C1".to_string(),
            cursor: "cursor-b".to_string(),
        })
        .unwrap();
        assert_eq!(first_page.target, second_page.target);
    }

    #[test]
    fn bounded_sync_navigation_plans_have_exact_contracts() {
        let history = connected_command_sync_plan(&RuntimeCommand::LoadHistory {
            channel_id: "C1".to_string(),
        })
        .unwrap();
        assert_eq!(
            history,
            RuntimeSyncPlan {
                target: runtime_sync_target(
                    SyncTargetKind::Conversation,
                    "conversation-history",
                    &["C1"],
                ),
                priority: SyncPriority::Interactive,
                durability: SyncDurability::Ephemeral,
                freshness: FreshnessPolicy::Always,
                replacement: ReplacementClass::Refresh(RefreshClass::ConversationHistory),
                retry: RetryPolicy::Never,
            }
        );

        let older = connected_command_sync_plan(&RuntimeCommand::LoadOlderHistory {
            channel_id: "C1".to_string(),
            cursor: "cursor-a".to_string(),
        })
        .unwrap();
        assert_eq!(older.target, history.target);
        assert_eq!(older.replacement, ReplacementClass::Never);

        let thread = connected_command_sync_plan(&RuntimeCommand::LoadThread {
            channel_id: "C1".to_string(),
            ts: "1710000000.000100".to_string(),
        })
        .unwrap();
        assert_eq!(thread.priority, SyncPriority::Interactive);
        assert_eq!(
            thread.replacement,
            ReplacementClass::Refresh(RefreshClass::ThreadReplies)
        );
        assert_eq!(
            thread.target,
            runtime_sync_target(
                SyncTargetKind::Thread,
                "thread-replies",
                &["C1", "1710000000.000100"],
            )
        );

        let main_context = connected_command_sync_plan(&RuntimeCommand::LoadMessageContext(
            SearchMessageLocation::new("C1", "1710000001.000100", None).unwrap(),
        ))
        .unwrap();
        assert_eq!(
            main_context.replacement,
            ReplacementClass::Refresh(RefreshClass::ConversationHistory)
        );
        assert_eq!(
            main_context.target,
            runtime_sync_target(
                SyncTargetKind::Conversation,
                "message-context",
                &["C1", "1710000001.000100"],
            )
        );

        let thread_context = connected_command_sync_plan(&RuntimeCommand::LoadMessageContext(
            SearchMessageLocation::new("C1", "1710000002.000100", Some("1710000000.000100"))
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(
            thread_context.replacement,
            ReplacementClass::Refresh(RefreshClass::ThreadReplies)
        );
        assert_eq!(
            thread_context.target,
            runtime_sync_target(
                SyncTargetKind::Thread,
                "message-context",
                &["C1", "1710000000.000100", "1710000002.000100"],
            )
        );

        for command in [
            RuntimeCommand::SearchMessages {
                query: "query".to_string(),
            },
            RuntimeCommand::LoadFiles,
            RuntimeCommand::LoadFile {
                file_id: "F1".to_string(),
                share_requested: false,
            },
            RuntimeCommand::LoadSavedItems,
        ] {
            let plan = connected_command_sync_plan(&command).unwrap();
            assert_eq!(plan.priority, SyncPriority::Interactive);
            assert_eq!(plan.durability, SyncDurability::Ephemeral);
            assert_eq!(plan.freshness, FreshnessPolicy::Always);
            assert_eq!(plan.replacement, ReplacementClass::Never);
            assert_eq!(plan.retry, RetryPolicy::Never);
        }
    }

    #[test]
    fn startup_sync_contracts_share_membership_replacement_with_manual_refresh() {
        let manual = connected_command_sync_plan(&RuntimeCommand::RefreshConversations).unwrap();
        let startup = startup_sync_plan(RuntimeStartupSyncKind::Membership);
        assert_eq!(manual.target, startup.target);
        assert_eq!(manual.replacement, startup.replacement);
        assert_eq!(
            startup,
            RuntimeSyncPlan {
                target: runtime_sync_target(
                    SyncTargetKind::Workspace,
                    "workspace-operation",
                    &["conversation-membership"],
                ),
                priority: SyncPriority::Foreground,
                durability: SyncDurability::Ephemeral,
                freshness: FreshnessPolicy::Always,
                replacement: ReplacementClass::Refresh(RefreshClass::Membership),
                retry: RetryPolicy::Never,
            }
        );

        let emoji = startup_sync_plan(RuntimeStartupSyncKind::EmojiCatalog);
        assert_eq!(emoji.priority, SyncPriority::Maintenance);
        assert_eq!(emoji.durability, SyncDurability::Ephemeral);
        assert_eq!(emoji.freshness, FreshnessPolicy::Always);
        assert_eq!(
            emoji.replacement,
            ReplacementClass::Refresh(RefreshClass::Workspace)
        );
        assert_eq!(emoji.retry, RetryPolicy::Never);

        let groups = startup_sync_plan(RuntimeStartupSyncKind::UserGroups);
        assert_eq!(groups.priority, SyncPriority::Maintenance);
        assert_eq!(
            groups.replacement,
            ReplacementClass::Refresh(RefreshClass::UserDirectory)
        );

        let automatic =
            connected_command_sync_plan(&RuntimeCommand::DiscoverConversations).unwrap();
        let manual_discovery =
            connected_command_sync_plan(&RuntimeCommand::DiscoverChannels).unwrap();
        assert_eq!(automatic.target, manual_discovery.target);
        assert_eq!(automatic.replacement, manual_discovery.replacement);
        assert_eq!(automatic.priority, SyncPriority::Maintenance);
        assert_eq!(manual_discovery.priority, SyncPriority::Interactive);
    }

    #[test]
    fn workspace_startup_network_work_has_no_legacy_spawn_or_lane_wait() {
        let source = include_str!("runtime.rs");
        let startup = source
            .split_once("fn spawn_workspace_tasks(")
            .and_then(|(_, tail)| tail.split_once("async fn load_cached_bootstrap("))
            .map(|(body, _)| body)
            .expect("spawn_workspace_tasks source slice should exist");

        assert!(!startup.contains("spawn_request_task("));
        assert!(!startup.contains(".acquire(RuntimeTaskLane::"));
        assert!(startup.contains("schedule_session_sync_work("));
    }

    #[test]
    fn conversation_star_command_is_an_interactive_channel_mutation() {
        let descriptor = RuntimeCommand::SetConversationStarred {
            channel_id: "C123".to_string(),
            starred: true,
        }
        .descriptor();

        assert_eq!(descriptor.lane, RuntimeTaskLane::Interactive);
        assert!(!descriptor.supersedes_previous);
        assert_eq!(
            descriptor.context,
            OperationContext::new(
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel("C123".to_string()),
            )
        );
    }

    #[test]
    fn current_user_status_command_is_an_interactive_user_mutation() {
        let descriptor = RuntimeCommand::SetCurrentUserStatus {
            status: SlackUserStatus {
                text: "Focus time".to_string(),
                emoji: ":headphones:".to_string(),
                expiration: 2_000_000_000,
            },
        }
        .descriptor();

        assert_eq!(descriptor.lane, RuntimeTaskLane::Interactive);
        assert!(!descriptor.supersedes_previous);
        assert_eq!(
            descriptor.context,
            OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace,)
        );
    }

    #[test]
    fn failed_conversation_star_persistence_emits_no_patch_or_completion() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-star-no-premature-event-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel("C1".to_string()),
            ),
            workspace_patch_send_gate: None,
        };

        runtime.block_on(async {
            let initial = SlackConversation {
                id: "C1".to_string(),
                is_channel: Some(true),
                is_starred: Some(false),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![initial.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversation(&store, &initial).await.unwrap();
            store
                .install_conversation_batch_failure_trigger_for("C1")
                .await
                .unwrap();

            assert!(persist_confirmed_conversation_star(
                &events,
                &workspace,
                Some(&store),
                "C1".to_string(),
                true,
            )
            .await
            .is_err());
            assert!(
                receiver.try_recv().is_err(),
                "neither WorkspacePatch nor ConversationStarUpdated may precede persistence"
            );
            assert!(workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred());
            assert!(!load_test_conversations(&store).await.unwrap().unwrap()[0].is_starred());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_star_sync_keeps_a_newer_toggle_after_a_stale_refresh() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-conversation-star-race-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T123:U123");
            let workspace = WorkspaceReducerAdapter::default();
            let initial = SlackConversation {
                id: "C1".to_string(),
                is_channel: Some(true),
                is_starred: Some(false),
                ..Default::default()
            };
            seed_test_conversation(&store, &initial).await.unwrap();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(initial.clone()),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };
            let gate = ConversationStarSyncGate::default();
            let refresh_guard = gate.lock().await;

            let toggle_gate = gate.clone();
            let toggle_store = store.clone();
            let toggle_workspace = workspace.clone();
            let toggle_events = events.clone();
            let toggle = tokio::spawn(async move {
                let _guard = toggle_gate.lock().await;
                persist_confirmed_conversation_star(
                    &toggle_events,
                    &toggle_workspace,
                    Some(&toggle_store),
                    "C1".to_string(),
                    true,
                )
                .await
                .unwrap();
            });
            tokio::task::yield_now().await;
            assert!(!toggle.is_finished());

            workspace.apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(initial.clone()),
            );
            drop(refresh_guard);
            toggle.await.unwrap();

            let persisted = load_test_conversations(&store)
                .await
                .unwrap()
                .expect("missing cached conversations");
            assert!(persisted[0].is_starred());
            assert!(workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred());
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::ConversationStarUpdated {
                    channel_id,
                    starred: true,
                } if channel_id == "C1"
            ));
            assert!(receiver.try_recv().is_err());

            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn huddle_actor_serializes_observation_and_user_commands() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender: event_sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Huddle,
                    RuntimeTarget::Huddle("active".to_string()),
                ),
                workspace_patch_send_gate: None,
            };
            let (handle, receiver) = huddle_actor_channel();
            let actor = tokio::spawn(run_huddle_actor(
                receiver,
                events,
                production_native_join_capability(false),
            ));
            let huddle = crate::huddles::model::ActiveHuddle {
                team_id: "T123".to_string(),
                channel_id: "C123".to_string(),
                call_id: "R123".to_string(),
                name: None,
                participant_ids: Vec::new(),
                started_at: None,
                huddle_link: None,
            };

            handle.observe_huddle(huddle).unwrap();
            let discovered = event_receiver.recv().await.unwrap();
            assert!(matches!(
                discovered.kind,
                RuntimeEventKind::Huddle(crate::huddles::state::HuddleEvent::Snapshot(snapshot))
                    if snapshot.phase == crate::huddles::state::HuddlePhase::Discovered
            ));

            handle
                .command(crate::huddles::state::HuddleCommand::OpenPreflight {
                    call_id: "R123".to_string(),
                })
                .unwrap();
            let preflight = event_receiver.recv().await.unwrap();
            assert!(matches!(
                preflight.kind,
                RuntimeEventKind::Huddle(crate::huddles::state::HuddleEvent::Snapshot(snapshot))
                    if snapshot.phase == crate::huddles::state::HuddlePhase::Preflight
            ));

            drop(handle);
            actor.await.unwrap();
        });
    }

    #[test]
    fn history_huddle_observation_is_chronological_and_workspace_scoped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender: event_sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Huddle,
                    RuntimeTarget::Huddle("active".to_string()),
                ),
                workspace_patch_send_gate: None,
            };
            let (handle, receiver) = huddle_actor_channel();
            let actor = tokio::spawn(run_huddle_actor(
                receiver,
                events,
                production_native_join_capability(false),
            ));
            let messages = serde_json::from_value::<Vec<SlackMessage>>(serde_json::json!([
                {
                    "ts": "2.0",
                    "room": {"id": "R123", "date_end": 2, "channels": ["C123"]}
                },
                {
                    "ts": "1.0",
                    "room": {
                        "id": "R123",
                        "date_start": 1,
                        "channels": ["C123"],
                        "participants": ["U123"]
                    }
                },
                {
                    "ts": "3.0",
                    "room": {"id": "R999", "channels": ["C999"]}
                }
            ]))
            .unwrap();

            observe_huddle_messages(&handle, Some("T123"), "C123", &messages);

            let discovered = event_receiver.recv().await.unwrap();
            assert!(matches!(
                discovered.kind,
                RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                    if snapshot.phase == HuddlePhase::Discovered
            ));
            let ended = event_receiver.recv().await.unwrap();
            assert!(matches!(
                ended.kind,
                RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                    if snapshot.phase == HuddlePhase::Idle
            ));
            assert!(event_receiver.try_recv().is_err());

            drop(handle);
            actor.await.unwrap();
        });
    }

    #[test]
    fn runtime_event_context_uses_loaded_resource_target() {
        let fallback = OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace);
        for event in [
            RuntimeEventKind::ConversationChannelsDiscovered(Vec::new()),
            RuntimeEventKind::ConversationPeopleDiscovered(Vec::new()),
        ] {
            assert_eq!(
                event.operation_context(&fallback),
                OperationContext::new(
                    RuntimeOperation::ConversationDiscovery,
                    RuntimeTarget::Workspace,
                )
            );
        }

        let event = RuntimeEventKind::HistoryLoaded {
            channel_id: "C123".to_string(),
            messages: Vec::new(),
            has_more: false,
            next_cursor: None,
            append_older: false,
            cached: false,
        };

        assert_eq!(
            event.operation_context(&fallback),
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C123".to_string()),
            )
        );

        let event = RuntimeEventKind::FileLoaded {
            file: Box::new(SlackFile {
                id: Some("F123".to_string()),
                ..Default::default()
            }),
            share_requested: false,
        };
        assert_eq!(
            event.operation_context(&fallback),
            OperationContext::new(
                RuntimeOperation::Files,
                RuntimeTarget::File("F123".to_string()),
            )
        );

        let event = RuntimeEventKind::MessageContextLoaded {
            location: SearchMessageLocation::new(
                "C123",
                "1710000001.000100",
                Some("1710000000.000100"),
            )
            .unwrap(),
            messages: Vec::new(),
        };
        assert_eq!(
            event.operation_context(&fallback),
            OperationContext::new(
                RuntimeOperation::Thread,
                RuntimeTarget::Thread {
                    channel_id: "C123".into(),
                    thread_ts: "1710000000.000100".into(),
                },
            )
        );

        let event = RuntimeEventKind::CurrentUserStatusUpdated {
            user_id: "U123".to_string(),
            status: Some(SlackUserStatus {
                text: "Focus time".to_string(),
                ..Default::default()
            }),
        };
        assert_eq!(
            event.operation_context(&fallback),
            OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace)
        );
    }

    #[test]
    fn socket_mode_reconnect_timing_backs_off_and_resets_after_socket_disconnects() {
        assert_eq!(
            socket_mode_reconnect_timing(SOCKET_MODE_INITIAL_RECONNECT_DELAY, None),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: Duration::from_secs(2),
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(Duration::from_secs(20), None),
            SocketModeReconnectTiming {
                sleep: Duration::from_secs(20),
                next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                SOCKET_MODE_MAX_RECONNECT_DELAY,
                Some(SocketModeDisconnect::LinkDisabled),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_MAX_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                SOCKET_MODE_MAX_RECONNECT_DELAY,
                Some(SocketModeDisconnect::RefreshRequested),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                Duration::from_secs(20),
                Some(SocketModeDisconnect::Warning),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            }
        );
    }

    #[test]
    fn realtime_membership_refresh_filters_other_users_but_keeps_workspace_signals() {
        assert!(socket_membership_refresh_required(
            &socket_mode::SocketModeConversationRefreshScope::Workspace,
            Some("U-current"),
        ));
        assert!(socket_membership_refresh_required(
            &socket_mode::SocketModeConversationRefreshScope::Membership {
                kind: socket_mode::SocketModeMembershipKind::Joined,
                user_id: "U-current".to_string(),
                channel_id: "C1".to_string(),
            },
            Some("U-current"),
        ));
        assert!(!socket_membership_refresh_required(
            &socket_mode::SocketModeConversationRefreshScope::Membership {
                kind: socket_mode::SocketModeMembershipKind::Left,
                user_id: "U-coworker".to_string(),
                channel_id: "C1".to_string(),
            },
            Some("U-current"),
        ));
        assert!(!socket_membership_refresh_required(
            &socket_mode::SocketModeConversationRefreshScope::Membership {
                kind: socket_mode::SocketModeMembershipKind::Joined,
                user_id: "U-current".to_string(),
                channel_id: "C1".to_string(),
            },
            None,
        ));
    }

    #[test]
    fn cached_conversation_user_ids_selects_known_direct_message_members() {
        let conversations = vec![
            SlackConversation {
                id: "D123".to_string(),
                user: Some("U123".to_string()),
                is_im: Some(true),
                ..Default::default()
            },
            SlackConversation {
                id: "D999".to_string(),
                user: Some("U999".to_string()),
                is_im: Some(true),
                ..Default::default()
            },
            SlackConversation {
                id: "C123".to_string(),
                user: Some("U123".to_string()),
                is_channel: Some(true),
                ..Default::default()
            },
        ];
        let user_cache = HashMap::from([("U123".to_string(), "Ada".to_string())]);

        assert_eq!(
            cached_conversation_user_ids(&conversations, &user_cache),
            vec!["U123"]
        );
    }

    fn channel(id: &str, unread_count: u64, last_read: Option<&str>) -> SlackConversation {
        let mut conversation = SlackConversation {
            id: id.to_string(),
            name: Some(
                id.trim_start_matches("C-")
                    .trim_start_matches('C')
                    .to_string(),
            ),
            is_channel: Some(true),
            unread_count: Some(unread_count),
            ..Default::default()
        };
        if let Some(last_read) = last_read {
            conversation
                .extra
                .insert("last_read".to_string(), serde_json::json!(last_read));
        }
        conversation
    }

    fn private_channel(id: &str, unread_count: u64, last_read: Option<&str>) -> SlackConversation {
        SlackConversation {
            is_channel: Some(false),
            is_group: Some(true),
            is_private: Some(true),
            ..channel(id, unread_count, last_read)
        }
    }

    fn archived_channel(id: &str, unread_count: u64) -> SlackConversation {
        SlackConversation {
            is_archived: Some(true),
            ..channel(id, unread_count, None)
        }
    }

    fn dm(id: &str, unread_count: u64) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            unread_count: Some(unread_count),
            ..Default::default()
        }
    }

    #[test]
    fn channel_history_prefetch_candidates_prioritize_unread_and_recent_channels() {
        let mut badgeless_unread = channel("C-badgeless", 0, Some("1710000100.000000"));
        badgeless_unread
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));
        let conversations = vec![
            channel("C-old", 0, None),
            dm("D-unread", 99),
            archived_channel("C-archived", 99),
            channel("C-recent", 0, Some("1710000300.000000")),
            channel("C-unread", 4, Some("1710000000.000000")),
            badgeless_unread,
            private_channel("G-private", 0, Some("1710000200.000000")),
        ];

        assert_eq!(
            channel_history_prefetch_candidates(&conversations),
            vec![
                "D-unread",
                "C-unread",
                "C-badgeless",
                "C-recent",
                "G-private",
                "C-old"
            ]
        );
    }

    #[test]
    fn channel_history_prefetch_candidates_are_bounded() {
        let conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
            .map(|index| channel(&format!("C{index}"), index as u64, None))
            .collect::<Vec<_>>();

        let candidates = channel_history_prefetch_candidates(&conversations);

        assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT);
        assert_eq!(candidates.first().map(String::as_str), Some("C14"));
        assert_eq!(candidates.last().map(String::as_str), Some("C3"));
    }

    #[test]
    fn channel_history_prefetch_always_includes_unread_direct_messages() {
        let mut conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
            .map(|index| channel(&format!("C{index}"), (index + 10) as u64, None))
            .collect::<Vec<_>>();
        conversations.push(dm("D-urgent", 1));

        let candidates = channel_history_prefetch_candidates(&conversations);

        assert_eq!(candidates.first().map(String::as_str), Some("D-urgent"));
        assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT + 1);
    }

    #[test]
    fn raw_unread_direct_message_is_prefetched_after_local_overlay_was_read() {
        let mut offline_unread = dm("D-offline", 2);
        offline_unread.observe_attention_message(false);
        assert!(!offline_unread.has_unread_activity());
        assert!(offline_unread.raw_has_unread_activity());

        assert_eq!(
            channel_history_prefetch_candidates(&[offline_unread.clone()]),
            ["D-offline"]
        );
        assert_eq!(
            conversation_unread_refresh_candidates(&[offline_unread]),
            ["D-offline"]
        );
    }

    #[test]
    fn channel_history_prefetch_always_includes_huddle_metadata() {
        let mut conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
            .map(|index| channel(&format!("C{index}"), (index + 10) as u64, None))
            .collect::<Vec<_>>();
        let mut huddle = channel("C-huddle", 0, None);
        huddle.extra.insert(
            "properties".to_string(),
            serde_json::json!({"huddles": [{"id": "R123"}]}),
        );
        conversations.push(huddle);

        let candidates = channel_history_prefetch_candidates(&conversations);

        assert_eq!(candidates.first().map(String::as_str), Some("C-huddle"));
        assert!(candidates.contains(&"C-huddle".to_string()));
    }

    #[test]
    fn channel_history_prefetch_keeps_current_huddles_after_cache_redaction() {
        let conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
            .map(|index| channel(&format!("C{index}"), (index + 10) as u64, None))
            .chain([channel("C-huddle", 0, None)])
            .collect::<Vec<_>>();
        let current_huddle_channels = HashSet::from(["C-huddle".to_string()]);

        let candidates = channel_history_prefetch_candidates_with_huddles(
            &conversations,
            &current_huddle_channels,
        );

        assert_eq!(candidates.first().map(String::as_str), Some("C-huddle"));
    }

    #[test]
    fn browser_unread_snapshot_covers_only_known_records_and_keeps_badges_boolean() {
        let conversations = vec![channel("C1", 0, None), dm("D1", 0), dm("D2", 0)];
        let raw = SlackUnreadSnapshot {
            channels: vec![SlackUnreadSnapshotRecord {
                conversation_id: "C1".to_string(),
                last_read: Some("10.0".to_string()),
                latest: Some("10.0".to_string()),
                has_unreads: false,
                mention_count: 0,
                is_open: false,
            }],
            ims: vec![
                SlackUnreadSnapshotRecord {
                    conversation_id: "D1".to_string(),
                    last_read: Some("10.0".to_string()),
                    latest: Some("11.0".to_string()),
                    has_unreads: true,
                    mention_count: 5,
                    is_open: true,
                },
                SlackUnreadSnapshotRecord {
                    conversation_id: "D-unknown".to_string(),
                    last_read: Some("10.0".to_string()),
                    latest: Some("11.0".to_string()),
                    has_unreads: true,
                    mention_count: 1,
                    is_open: true,
                },
            ],
            mpims: Vec::new(),
        };

        let (snapshots, covered) = browser_unread_snapshots_for_catalog(raw, &conversations);

        assert_eq!(covered, HashSet::from(["C1".to_string(), "D1".to_string()]));
        assert_eq!(snapshots.len(), 2);
        let direct_message = snapshots
            .iter()
            .find(|snapshot| snapshot.channel_id == "D1")
            .unwrap();
        assert!(direct_message.unread_state.has_unread);
        assert_eq!(direct_message.unread_state.display_count, 0);
        assert_eq!(direct_message.mention_count, Some(5));
        assert_eq!(direct_message.is_open, Some(true));
        assert_eq!(
            uncovered_conversation_unread_refresh_candidates(&conversations, &covered),
            vec!["D2"]
        );
    }

    #[test]
    fn conversation_refresh_batches_bound_their_potential_patch_count() {
        let mut pending = PendingConversationRefreshBatch::default();
        let mut ready = Vec::new();
        for index in 0..11 {
            let channel_id = format!("C{index}");
            if let Some(batch) = pending.push(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                ConversationRefresh {
                    metadata: Some(SlackConversation {
                        id: channel_id.clone(),
                        ..Default::default()
                    }),
                    unread: Some(SlackConversationUnreadSnapshot {
                        channel_id,
                        unread_state: SlackUnreadState::from_parts(true, true, 1),
                        ..Default::default()
                    }),
                },
            )) {
                ready.push(batch);
            }
        }

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].len(), 10);
        assert_eq!(
            ready[0]
                .iter()
                .map(|refresh| refresh.data().potential_change_count())
                .sum::<usize>(),
            CONVERSATION_PATCH_BATCH_SIZE
        );
        assert_eq!(pending.refreshes.len(), 1);
        assert_eq!(pending.potential_changes, 2);
    }

    #[test]
    fn conversation_unread_refresh_candidates_prioritize_dm_state_before_known_unread_channels() {
        let conversations = vec![
            channel("C-zebra", 0, None),
            archived_channel("C-archived", 10),
            dm("D-ada", 4),
            channel("C-aggregator", 0, None),
            channel("C-127", 0, None),
        ];

        assert_eq!(
            conversation_unread_refresh_candidates(&conversations),
            vec!["D-ada", "C-127", "C-aggregator", "C-zebra"]
        );

        let mut many = (0..CONVERSATION_ENRICHMENT_LIMIT + 5)
            .map(|index| channel(&format!("C{index}"), 10, None))
            .collect::<Vec<_>>();
        let unknown_dm = SlackConversation {
            id: "D-unknown".to_string(),
            user: Some("U-unknown".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let mut active_unknown_dm = SlackConversation {
            id: "D-active".to_string(),
            user: Some("U-active".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        active_unknown_dm
            .extra
            .insert("priority".to_string(), serde_json::json!(0.75));
        let known_unread_dm = dm("D-unread", 1);
        let mut active_known_read_count = dm("D-active-read-count", 0);
        active_known_read_count
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
        let mut active_known_read_flag = SlackConversation {
            id: "D-active-read-flag".to_string(),
            user: Some("U-read-flag".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        active_known_read_flag
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(false));
        active_known_read_flag
            .extra
            .insert("priority".to_string(), serde_json::json!(0.5));
        assert!(!unknown_dm.unread_state().known);
        assert!(active_known_read_count.unread_state().known);
        assert!(active_known_read_flag.unread_state().known);
        assert!(!active_known_read_flag.unread_state().has_unread);
        many.extend([
            unknown_dm,
            active_unknown_dm,
            known_unread_dm,
            active_known_read_count,
            active_known_read_flag,
        ]);

        let candidates = conversation_unread_refresh_candidates(&many);
        assert_eq!(candidates.first().map(String::as_str), Some("D-active"));
        assert_eq!(candidates.get(1).map(String::as_str), Some("D-unknown"));
        assert_eq!(candidates.get(2).map(String::as_str), Some("D-unread"));
        let unread_dm_index = candidates
            .iter()
            .position(|channel_id| channel_id == "D-unread")
            .unwrap();
        for active_read_id in ["D-active-read-count", "D-active-read-flag"] {
            assert!(
                unread_dm_index
                    < candidates
                        .iter()
                        .position(|channel_id| channel_id == active_read_id)
                        .unwrap()
            );
        }
        let plan = conversation_unread_refresh_plan(
            Vec::new(),
            candidates.clone(),
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(plan.batch.len(), CONVERSATION_ENRICHMENT_LIMIT);
        assert!(plan.batch.contains(&"D-active".to_string()));
        assert!(plan.batch.contains(&"D-unknown".to_string()));
        assert_eq!(plan.queue.len(), candidates.len());
        assert_eq!(plan.next_queue.len(), candidates.len());
    }

    #[test]
    fn conversation_priority_ignores_non_finite_values() {
        for priority in ["NaN", "inf", "-inf"] {
            let mut conversation = dm("D1", 0);
            conversation
                .extra
                .insert("priority".to_string(), serde_json::json!(priority));

            assert_eq!(conversation.priority_hint(), 0.0);
        }
    }

    #[test]
    fn conversation_unread_refresh_plan_rotates_every_candidate_without_starvation() {
        let candidates = (0..CONVERSATION_ENRICHMENT_LIMIT * 2 + 5)
            .map(|index| format!("C{index:02}"))
            .collect::<Vec<_>>();
        let first = conversation_unread_refresh_plan(
            Vec::new(),
            candidates.clone(),
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(first.batch, candidates[..CONVERSATION_ENRICHMENT_LIMIT]);

        let second = conversation_unread_refresh_plan(
            first.next_queue.clone(),
            candidates.clone(),
            CONVERSATION_ENRICHMENT_LIMIT,
        );

        assert_eq!(
            second.batch,
            candidates[CONVERSATION_ENRICHMENT_LIMIT..CONVERSATION_ENRICHMENT_LIMIT * 2]
        );
        assert!(!second.batch.contains(&candidates[0]));
        assert!(second.batch.len() <= CONVERSATION_ENRICHMENT_LIMIT);

        let third = conversation_unread_refresh_plan(
            second.next_queue.clone(),
            candidates.clone(),
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(
            third.batch,
            candidates[CONVERSATION_ENRICHMENT_LIMIT * 2..]
                .iter()
                .chain(&candidates[..CONVERSATION_ENRICHMENT_LIMIT - 5])
                .cloned()
                .collect::<Vec<_>>()
        );
        let visited = first
            .batch
            .iter()
            .chain(&second.batch)
            .chain(&third.batch)
            .collect::<HashSet<_>>();
        assert_eq!(visited.len(), candidates.len());
        assert!(candidates
            .iter()
            .all(|channel_id| visited.contains(channel_id)));

        let mut with_new_candidate = vec!["D-new-active".to_string()];
        with_new_candidate.extend(candidates.clone());
        let with_new = conversation_unread_refresh_plan(
            first.next_queue.clone(),
            with_new_candidate,
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(
            with_new.batch.first().map(String::as_str),
            Some("D-new-active")
        );

        let mut reranked = candidates.clone();
        reranked.rotate_right(1);
        let reranked_existing = conversation_unread_refresh_plan(
            first.next_queue,
            reranked,
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(
            reranked_existing.batch.first().map(String::as_str),
            Some("C30")
        );
    }

    #[test]
    fn conversation_unread_refresh_plan_prunes_and_deduplicates_pending_work() {
        let plan = conversation_unread_refresh_plan(
            vec![
                "C2".to_string(),
                "C-stale".to_string(),
                "C2".to_string(),
                String::new(),
                "C1".to_string(),
            ],
            vec![
                "C1".to_string(),
                String::new(),
                "C2".to_string(),
                "C1".to_string(),
                "C3".to_string(),
            ],
            2,
        );

        assert_eq!(plan.queue, vec!["C3", "C2", "C1"]);
        assert_eq!(plan.batch, vec!["C3", "C2"]);
        assert_eq!(plan.next_queue, vec!["C1", "C3", "C2"]);
        assert_eq!(
            plan.queue.iter().collect::<HashSet<_>>().len(),
            plan.queue.len()
        );

        let no_network = conversation_unread_refresh_plan(
            Vec::new(),
            vec!["C1".to_string(), "C2".to_string()],
            0,
        );
        assert!(no_network.batch.is_empty());
        assert_eq!(no_network.next_queue, no_network.queue);

        let under_limit = conversation_unread_refresh_plan(
            Vec::new(),
            vec!["C1".to_string(), "C2".to_string()],
            CONVERSATION_ENRICHMENT_LIMIT,
        );
        assert_eq!(under_limit.batch, under_limit.queue);
        assert_eq!(under_limit.next_queue, under_limit.queue);
    }

    #[test]
    fn membership_snapshot_uses_raw_api_state_and_returns_the_coordinator_projection() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-workspace-raw-membership-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let workspace = WorkspaceReducerAdapter::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut cached = channel("C1", 5, Some("10.0"));
            cached.name = Some("cached".to_string());
            cached.is_starred = Some(false);
            cached
                .extra
                .insert("topic".to_string(), serde_json::json!("Cached topic"));
            cached
                .extra
                .insert("unread_count_display".to_string(), serde_json::json!(3));
            let removed = SlackConversation {
                id: "C_OLD".to_string(),
                name: Some("removed".to_string()),
                is_channel: Some(true),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![cached.clone(), removed.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            seed_test_conversations(&store, &[cached.clone(), removed])
                .await
                .unwrap();
            let base_revision = workspace.revision();

            for (origin, conversation) in [
                (
                    MutationOrigin::Local,
                    SlackConversation {
                        id: "C_LOCAL".to_string(),
                        name: Some("joined locally".to_string()),
                        is_channel: Some(true),
                        ..Default::default()
                    },
                ),
                (
                    MutationOrigin::Realtime,
                    SlackConversation {
                        id: "C_REALTIME".to_string(),
                        name: Some("realtime membership".to_string()),
                        is_channel: Some(true),
                        ..Default::default()
                    },
                ),
            ] {
                workspace
                    .apply_persisted(
                        Some(&store),
                        origin,
                        WorkspaceMutation::ConversationUpsert(conversation),
                    )
                    .await
                    .unwrap();
            }

            let fresh = vec![
                SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    is_channel: Some(true),
                    is_starred: Some(false),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".to_string(),
                    name: Some("new".to_string()),
                    is_channel: Some(true),
                    is_starred: Some(true),
                    ..Default::default()
                },
            ];
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: Some(RequestId::new(7)),
                fallback: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };

            let conversations = apply_conversation_membership_snapshot(
                &events,
                Some(&store),
                &workspace,
                base_revision,
                fresh,
                Some(HashSet::from(["C1".to_string(), "C_LOCAL".to_string()])),
            )
            .await
            .unwrap();

            assert_eq!(
                conversations
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["C1", "C2", "C_LOCAL", "C_REALTIME"]
            );
            let current = conversations
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(current.name.as_deref(), Some("renamed"));
            assert!(current.is_starred());
            assert_eq!(current.unread_activity_count(), 5);
            assert_eq!(current.unread_state().display_count, 3);
            assert_eq!(current.extra["topic"], serde_json::json!("Cached topic"));
            assert!(conversations
                .iter()
                .find(|conversation| conversation.id == "C_LOCAL")
                .unwrap()
                .is_starred());
            assert!(!conversations
                .iter()
                .find(|conversation| conversation.id == "C2")
                .unwrap()
                .is_starred());
            let mut persisted = load_test_conversations(&store).await.unwrap().unwrap();
            persisted.sort_by(|left, right| left.id.cmp(&right.id));
            assert_eq!(persisted, conversations);

            let event = receiver.recv().await.unwrap();
            assert_eq!(event.meta.request, None);
            assert!(matches!(
                event.kind,
                RuntimeEventKind::WorkspacePatch(ref patch)
                    if patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ConversationRemoved { channel_id }
                            if channel_id == "C_OLD"
                    ))
            ));
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::ConversationsSynchronized
            ));
            assert!(receiver.try_recv().is_err());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn suspicious_empty_raw_membership_snapshot_does_not_erase_coordinator_state() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            let retained = channel("C1", 0, None);
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![retained.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };
            assert!(apply_conversation_membership_snapshot(
                &events,
                None,
                &workspace,
                workspace.revision(),
                vec![SlackConversation {
                    id: "   ".to_string(),
                    ..Default::default()
                }],
                Some(HashSet::new()),
            )
            .await
            .is_err());
            assert_eq!(workspace.conversations(), vec![retained]);
            assert!(receiver.try_recv().is_err());

            let empty = WorkspaceReducerAdapter::default();
            assert!(apply_conversation_membership_snapshot(
                &events,
                None,
                &empty,
                empty.revision(),
                Vec::new(),
                Some(HashSet::new()),
            )
            .await
            .unwrap()
            .is_empty());
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::ConversationsSynchronized
            ));
            assert!(receiver.try_recv().is_err());
        });
    }

    #[test]
    fn unchanged_membership_snapshot_emits_only_a_state_free_completion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            let conversation = SlackConversation {
                id: "C1".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                ..Default::default()
            };
            workspace
                .apply(
                    MutationOrigin::Cache,
                    WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                        conversations: vec![conversation.clone()],
                        ..Default::default()
                    }),
                )
                .unwrap();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: Some(RequestId::new(3)),
                fallback: OperationContext::new(
                    RuntimeOperation::Conversations,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };

            assert_eq!(
                apply_conversation_membership_snapshot(
                    &events,
                    None,
                    &workspace,
                    workspace.revision(),
                    vec![conversation.clone()],
                    None,
                )
                .await
                .unwrap(),
                vec![conversation]
            );
            let event = receiver.recv().await.unwrap();
            assert_eq!(event.meta.request, Some(RequestId::new(3)));
            assert!(matches!(
                event.kind,
                RuntimeEventKind::ConversationsSynchronized
            ));
            assert!(receiver.try_recv().is_err());
        });
    }

    #[test]
    fn preview_asset_cache_round_trips_image_and_video_data_uris() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-image-cache-test-{}-{unique}",
            std::process::id()
        ));
        let cache = ImageAssetCache::new(directory.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            assert_eq!(
                cache
                    .load("https://files.example/image.png")
                    .await
                    .expect("cache load failed"),
                None
            );

            cache
                .store(
                    "https://files.example/image.png",
                    "data:image/png;base64,abc",
                )
                .await
                .expect("cache store failed");

            assert_eq!(
                cache
                    .load("https://files.example/image.png")
                    .await
                    .expect("cache load failed")
                    .as_deref(),
                Some("data:image/png;base64,abc")
            );

            cache
                .store(
                    "https://files.example/video.mp4",
                    "data:video/mp4;base64,def",
                )
                .await
                .expect("cache store failed");
            assert_eq!(
                cache
                    .load("https://files.example/video.mp4")
                    .await
                    .expect("cache load failed")
                    .as_deref(),
                Some("data:video/mp4;base64,def")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn local_reaction_confirmation_survives_store_failure_and_recovers_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-local-reaction-authority-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let message = SlackMessage {
                ts: "1.0".into(),
                text: Some("hello".into()),
                ..Default::default()
            };
            seed_test_history(&store, "C1", std::slice::from_ref(&message))
                .await
                .unwrap();
            store
                .install_history_batch_failure_trigger_for("C1")
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), vec![message])]),
                    ..Default::default()
                }),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::Reaction,
                    RuntimeTarget::Message {
                        channel_id: "C1".into(),
                        thread_ts: None,
                    },
                ),
            );

            persist_confirmed_reaction(
                &events,
                &workspace,
                Some(&store),
                ReactionMutation {
                    channel_id: "C1".into(),
                    message_ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U_SELF".into(),
                    added: true,
                },
                None,
            )
            .await;

            let completion = receiver.recv().await.unwrap();
            assert!(matches!(
                completion.kind,
                RuntimeEventKind::ReactionUpdated {
                    channel_id,
                    ts,
                    name,
                    added: true,
                    thread_ts: None,
                } if channel_id == "C1" && ts == "1.0" && name == "wave"
            ));
            assert!(
                receiver.try_recv().is_err(),
                "a failed store batch must withhold only the typed patch"
            );
            assert!(!message_has_wave(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .first()
                    .unwrap()
            ));

            store.clear_history_batch_failure_trigger().await.unwrap();
            persist_realtime_reaction(
                &store,
                &events,
                &workspace,
                socket_mode::SocketModeReactionEvent {
                    channel_id: "C1".into(),
                    ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U_SELF".into(),
                    added: true,
                },
            )
            .await;

            let recovered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert_eq!(
                recovered
                    .iter()
                    .filter(|kind| matches!(kind, RuntimeEventKind::ReactionUpdated { .. }))
                    .count(),
                0,
                "recovery and the realtime echo must not repeat local completion"
            );
            assert_eq!(
                recovered
                    .iter()
                    .filter(|kind| {
                        matches!(
                            kind,
                            RuntimeEventKind::WorkspacePatch(patch)
                                if patch.changes().iter().any(workspace_change_has_wave)
                        )
                    })
                    .count(),
                1
            );
            assert_eq!(
                recovered
                    .iter()
                    .filter(|kind| {
                        matches!(
                            kind,
                            RuntimeEventKind::SocketModeEvent {
                                event: SocketModeEvent::Reaction(_),
                                ..
                            }
                        )
                    })
                    .count(),
                1
            );

            let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let persisted = reopened
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .find(|message| message.ts == "1.0")
                .unwrap();
            assert!(message_has_wave(&persisted));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn realtime_reaction_normalizes_to_the_same_coordinator_mutation() {
        let event = socket_mode::SocketModeReactionEvent {
            channel_id: "C1".into(),
            ts: "1.0".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: true,
        };

        assert!(matches!(
            realtime_reaction_mutation(&event),
            WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id,
                message_ts,
                name,
                user_id,
                added: true,
            }) if channel_id == "C1"
                && message_ts == "1.0"
                && name == "wave"
                && user_id == "U1"
        ));
    }

    #[test]
    fn realtime_reaction_without_store_publishes_raw_before_typed_patch() {
        let workspace = WorkspaceReducerAdapter::default();
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([(
                    "C1".into(),
                    vec![SlackMessage {
                        ts: "1.0".into(),
                        text: Some("hello".into()),
                        ..Default::default()
                    }],
                )]),
                ..Default::default()
            }),
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(publish_realtime_reaction_without_store(
                &workspace,
                &events,
                SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                    channel_id: "C1".into(),
                    ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U1".into(),
                    added: true,
                }),
            ));

        assert!(matches!(
            receiver.try_recv().unwrap().kind,
            RuntimeEventKind::SocketModeEvent {
                event: SocketModeEvent::Reaction(_),
                attention: None,
            }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().kind,
            RuntimeEventKind::WorkspacePatch(ref patch)
                if patch.changes().iter().any(workspace_change_has_wave)
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn no_store_reaction_publication_serializes_raw_and_patch_pairs() {
        let workspace = WorkspaceReducerAdapter::default();
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([(
                    "C1".into(),
                    vec![SlackMessage {
                        ts: "1.0".into(),
                        text: Some("hello".into()),
                        ..Default::default()
                    }],
                )]),
                ..Default::default()
            }),
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let session = SessionId::default().next();
        let (patch_started, patch_reached) = std::sync::mpsc::channel();
        let (release_patch, release) = std::sync::mpsc::channel();
        let mut first_events = RuntimeEventSender::new(
            sender.clone(),
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        first_events.workspace_patch_send_gate = Some(Arc::new(TestWorkspacePatchSendGate {
            started: patch_started,
            release: Mutex::new(release),
        }));
        let second_events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );

        let first_workspace = workspace.clone();
        let first = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(publish_realtime_reaction_without_store(
                    &first_workspace,
                    &first_events,
                    SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                        channel_id: "C1".into(),
                        ts: "1.0".into(),
                        name: "wave".into(),
                        user_id: "U1".into(),
                        added: true,
                    }),
                ));
        });
        patch_reached
            .recv_timeout(Duration::from_secs(5))
            .expect("the first reaction patch did not reach its publication gate");

        let second_workspace = workspace.clone();
        let (second_finished, second_completion) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(publish_realtime_reaction_without_store(
                    &second_workspace,
                    &second_events,
                    SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                        channel_id: "C1".into(),
                        ts: "1.0".into(),
                        name: "heart".into(),
                        user_id: "U2".into(),
                        added: true,
                    }),
                ));
            second_finished.send(()).unwrap();
        });
        assert!(
            second_completion
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "a later raw/patch pair must not pass a blocked earlier pair"
        );

        release_patch.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        second_completion.recv().unwrap();

        let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert!(matches!(
            delivered.as_slice(),
            [
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Reaction(first),
                    ..
                },
                RuntimeEventKind::WorkspacePatch(first_patch),
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Reaction(second),
                    ..
                },
                RuntimeEventKind::WorkspacePatch(second_patch),
            ] if first.name == "wave"
                && second.name == "heart"
                && workspace_patch_has_reaction(first_patch, "wave")
                && workspace_patch_has_reaction(second_patch, "heart")
        ));
    }

    #[test]
    fn no_store_reaction_waits_for_an_older_non_reaction_patch_publication() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([(
                        "C1".into(),
                        vec![SlackMessage {
                            ts: "1.0".into(),
                            text: Some("hello".into()),
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                }),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let session = SessionId::default().next();
            let (patch_started, patch_reached) = std::sync::mpsc::channel();
            let (release_patch, release) = std::sync::mpsc::channel();
            let mut older_events = RuntimeEventSender::new(
                sender.clone(),
                RuntimeIdentity {
                    session,
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            );
            older_events.workspace_patch_send_gate = Some(Arc::new(TestWorkspacePatchSendGate {
                started: patch_started,
                release: Mutex::new(release),
            }));
            let reaction_events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session,
                    request: RequestId::new(2),
                },
                OperationContext::new(
                    RuntimeOperation::Reaction,
                    RuntimeTarget::Message {
                        channel_id: "C1".into(),
                        thread_ts: None,
                    },
                ),
            );

            let older_workspace = workspace.clone();
            let older = tokio::spawn(async move {
                older_workspace
                    .apply_persisted_and_publish(
                        None,
                        &older_events,
                        MutationOrigin::WebApi,
                        WorkspaceMutation::ConversationUpsert(SlackConversation {
                            id: "C2".into(),
                            name: Some("older".into()),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap();
            });
            patch_reached
                .recv_timeout(Duration::from_secs(5))
                .expect("the older patch did not reach its publication gate");

            let reaction_workspace = workspace.clone();
            let reaction = tokio::spawn(async move {
                persist_confirmed_reaction(
                    &reaction_events,
                    &reaction_workspace,
                    None,
                    ReactionMutation {
                        channel_id: "C1".into(),
                        message_ts: "1.0".into(),
                        name: "wave".into(),
                        user_id: "U_SELF".into(),
                        added: true,
                    },
                    None,
                )
                .await;
            });
            assert!(
                tokio::time::timeout(Duration::from_millis(50), receiver.recv())
                    .await
                    .is_err(),
                "a newer reaction completion or patch must not pass an older blocked publisher"
            );

            release_patch.send(()).unwrap();
            older.await.unwrap();
            reaction.await.unwrap();

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(matches!(
                delivered.as_slice(),
                [
                    RuntimeEventKind::WorkspacePatch(older_patch),
                    RuntimeEventKind::ReactionUpdated { .. },
                    RuntimeEventKind::WorkspacePatch(reaction_patch),
                ] if older_patch.revision() < reaction_patch.revision()
            ));
        });
    }

    #[test]
    fn local_reaction_publishes_raw_completion_before_its_patch() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            let mut root = SlackMessage {
                ts: "1.0".into(),
                text: Some("root".into()),
                reply_count: Some(1),
                ..Default::default()
            };
            root.refresh_canonical_content();
            let mut catalog = crate::thread_catalog::ThreadCatalog::default();
            catalog.observe_thread("C1", "1.0", std::slice::from_ref(&root), false);
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    threads: catalog.into_records(),
                    ..Default::default()
                }),
            );
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(
                    RuntimeOperation::Reaction,
                    RuntimeTarget::Message {
                        channel_id: "C1".into(),
                        thread_ts: Some("1.0".into()),
                    },
                ),
            );

            persist_confirmed_reaction(
                &events,
                &workspace,
                None,
                ReactionMutation {
                    channel_id: "C1".into(),
                    message_ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U_SELF".into(),
                    added: true,
                },
                Some("1.0".into()),
            )
            .await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(matches!(
                delivered.as_slice(),
                [
                    RuntimeEventKind::ReactionUpdated { .. },
                    RuntimeEventKind::WorkspacePatch(patch),
                ] if patch.changes().iter().any(|change| {
                    matches!(
                        change,
                        WorkspaceChange::ThreadCatalogChanged(records)
                            if message_has_named_reaction(
                                records[0].root.as_ref().unwrap(),
                                "wave"
                            )
                    )
                })
            ));
        });
    }

    #[test]
    fn closed_worker_last_reaction_recovers_without_a_later_event() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-reaction-fallback-admission-{}-{nonce}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
            let message = SlackMessage {
                ts: "1.0".into(),
                text: Some("hello".into()),
                ..Default::default()
            };
            seed_test_history(&store, "C1", std::slice::from_ref(&message))
                .await
                .unwrap();
            let workspace = WorkspaceReducerAdapter::default();
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    histories: HashMap::from([("C1".into(), vec![message])]),
                    ..Default::default()
                }),
            );
            let base_revision = workspace.revision();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
            );

            let fallback = RealtimePersistenceFallback::new(
                Some(store.clone()),
                Some("U_SELF".into()),
                events.clone(),
                workspace.clone(),
                UserStatusSync::default(),
            );
            let admission = workspace.publication_admission.lock().await;
            fallback.schedule(RealtimePersistenceEvent::OrderedEvent {
                event: SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                    channel_id: "C1".into(),
                    ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U1".into(),
                    added: true,
                }),
            });
            assert_eq!(
                workspace.revision(),
                base_revision,
                "the synchronous fallback must defer coordinator admission"
            );
            assert!(
                receiver.try_recv().is_err(),
                "the raw event must stay paired with persistence admission"
            );
            drop(admission);
            fallback.drain().await;

            let raw = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("the fallback reaction was not recovered autonomously")
                .expect("the runtime event stream closed");
            assert!(matches!(
                raw.kind,
                RuntimeEventKind::SocketModeEvent {
                    event: SocketModeEvent::Reaction(ref event),
                    ..
                } if event.name == "wave"
            ));
            let patch = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("the fallback reaction patch was not published")
                .expect("the runtime event stream closed");
            assert!(matches!(
                patch.kind,
                RuntimeEventKind::WorkspacePatch(ref patch)
                    if workspace_patch_has_reaction(patch, "wave")
            ));
            assert!(receiver.try_recv().is_err());
            let persisted = store.load_history("C1").await.unwrap().unwrap();
            assert!(message_has_named_reaction(&persisted[0], "wave"));
            let _ = std::fs::remove_dir_all(directory);
        });
    }

    #[test]
    fn reaction_command_requires_identity_before_the_slack_call() {
        let source = include_str!("runtime.rs");
        let command = source
            .split_once("RuntimeCommand::SetReaction {")
            .unwrap()
            .1
            .split_once("RuntimeCommand::SetSaved {")
            .unwrap()
            .0;
        let identity_check = command
            .find("current Slack user identity is unavailable")
            .unwrap();
        let slack_call = command.find("api.set_reaction(").unwrap();
        assert!(identity_check < slack_call);
        assert!(!command.contains("unwrap_or_default()"));
    }

    fn workspace_change_has_wave(change: &WorkspaceChange) -> bool {
        let WorkspaceChange::TimelineChanged { changes, .. } = change else {
            return false;
        };
        changes.iter().any(|change| {
            let crate::workspace_pipeline::MessageChange::Upsert(message) = change else {
                return false;
            };
            message_has_wave(message)
        })
    }

    fn message_has_wave(message: &SlackMessage) -> bool {
        message_has_named_reaction(message, "wave")
    }

    fn workspace_patch_has_reaction(patch: &WorkspacePatch, name: &str) -> bool {
        patch.changes().iter().any(|change| {
            let WorkspaceChange::TimelineChanged { changes, .. } = change else {
                return false;
            };
            changes.iter().any(|change| {
                let crate::workspace_pipeline::MessageChange::Upsert(message) = change else {
                    return false;
                };
                message_has_named_reaction(message, name)
            })
        })
    }

    fn message_has_named_reaction(message: &SlackMessage, name: &str) -> bool {
        matches!(
            message.reactions.as_deref(),
            Some([reaction])
                if reaction.name.as_deref() == Some(name)
                    && reaction.count == Some(1)
                    && reaction.users.as_ref().is_some_and(|users| users.len() == 1)
        ) || message.reactions.as_ref().is_some_and(|reactions| {
            reactions.iter().any(|reaction| {
                reaction.name.as_deref() == Some(name)
                    && reaction.count == Some(1)
                    && reaction
                        .users
                        .as_ref()
                        .is_some_and(|users| users.len() == 1)
            })
        })
    }
}
