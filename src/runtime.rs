use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
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
    MessageControlHandle, MessageHandoffResolver, MessageRef, ProviderFailure,
    ResolvedMessageHandoff,
};
use crate::models::{
    slack_timestamp_is_after, AuthInfo, SavedItem, SearchMatch, SearchMessageLocation,
    SlackConversation, SlackConversationUnreadSnapshot, SlackFile, SlackMessage, SlackUnreadState,
    SlackUser, SlackUserStatus, StoredToken,
};
use crate::realtime::RealtimeStatus;
use crate::services::conversation_history::ConversationHistoryService;
use crate::slack::{
    DownloadedPreviewAsset, PreviewAssetMime, SlackApi, SlackError, SlackErrorCategory,
    SlackMessageActionRequest, SlackUnreadSnapshot, SlackUnreadSnapshotRecord,
};
use crate::socket_mode::{self, SocketModeDisconnect, SocketModeEvent, SocketModeMessageKind};
use crate::store::{
    AttentionObservationStatus, StoreError, StoreErrorCategory, SyncFreshness, WorkspaceBootstrap,
    WorkspaceStore,
};
use crate::sync_scheduler::{
    AdmissionOutcome, CancellationId, CompletionOutcome, FreshnessPolicy, JobOutcome, RefreshClass,
    ReplacementClass, RetryPolicy, SchedulerConfig, SyncDurability, SyncJob, SyncJobId,
    SyncPriority, SyncScheduler, SyncTargetKey, SyncTargetKind,
};
use crate::workspace_pipeline::{
    ConversationMembershipSnapshot, ConversationRefresh, MessageAttentionEffect,
    MessageMutationKind, MutationOrigin, SnapshotEnvelope, StoreBatch, StoreChange,
    WorkspaceAttentionContext, WorkspaceBootstrapData, WorkspaceCoordinator, WorkspaceEffect,
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
const REALTIME_PERSISTENCE_QUEUE_CAPACITY: usize = 256;
const SOCKET_MODE_INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const SOCKET_MODE_MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const ATTACHMENT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const ATTACHMENT_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const ATTACHMENT_BASENAME_MAX_BYTES: usize = 180;
const PREVIEW_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PREVIEW_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;
const PREVIEW_CACHE_MAX_ENTRIES: usize = 16_384;
const PREVIEW_VALIDATION_PREFIX_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadAttachment {
    pub path: PathBuf,
    pub remove_after_upload: bool,
}

#[derive(Debug)]
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
    ExecuteMessageAction {
        request: SlackMessageActionRequest,
        control_handle: MessageControlHandle,
    },
    MarkConversationRead {
        channel_id: String,
        ts: String,
    },
    MarkConversationReadAll {
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
        blocks_json: Option<String>,
        thread_ts: Option<String>,
    },
    UpdateMessage {
        channel_id: String,
        original: Box<SlackMessage>,
        text: String,
        blocks_json: Option<String>,
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
    UploadFiles {
        channel_id: String,
        thread_ts: Option<String>,
        attachments: Vec<UploadAttachment>,
        blocks_json: Option<String>,
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
    MessageAction,
    PostMessage,
    UpdateMessage,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeAdmissionKind {
    Control,
    DurableAction,
    ReadMarker,
    Coalescible,
    Supersedable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ConversationDiscoveryScope {
    Full,
    Channels,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum UserLoadScope {
    Basic,
    Profile,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct OpaqueAdmissionTarget([u8; 32]);

impl OpaqueAdmissionTarget {
    fn digest(parts: &[&str]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            let length = u64::try_from(part.len()).expect("runtime admission target is too large");
            hasher.update(length.to_be_bytes());
            hasher.update(part.as_bytes());
        }
        Self(hasher.finalize().into())
    }
}

impl std::fmt::Debug for OpaqueAdmissionTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpaqueAdmissionTarget")
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RuntimeAdmissionKey {
    Authentication,
    Navigation(NavigationSlot),
    WorkspaceRefresh,
    ConversationDiscovery(ConversationDiscoveryScope),
    User {
        scope: UserLoadScope,
        target: OpaqueAdmissionTarget,
    },
    ImageAsset(OpaqueAdmissionTarget),
    Media(OpaqueAdmissionTarget),
    MessagePermalink(OpaqueAdmissionTarget),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeAdmissionPolicy {
    kind: RuntimeAdmissionKind,
    replacement_key: Option<RuntimeAdmissionKey>,
}

impl RuntimeAdmissionPolicy {
    fn control() -> Self {
        Self {
            kind: RuntimeAdmissionKind::Control,
            replacement_key: None,
        }
    }

    fn durable_action() -> Self {
        Self {
            kind: RuntimeAdmissionKind::DurableAction,
            replacement_key: None,
        }
    }

    fn read_marker() -> Self {
        Self {
            kind: RuntimeAdmissionKind::ReadMarker,
            replacement_key: None,
        }
    }

    fn coalescible(replacement_key: RuntimeAdmissionKey) -> Self {
        Self {
            kind: RuntimeAdmissionKind::Coalescible,
            replacement_key: Some(replacement_key),
        }
    }

    fn supersedable(replacement_key: RuntimeAdmissionKey) -> Self {
        Self {
            kind: RuntimeAdmissionKind::Supersedable,
            replacement_key: Some(replacement_key),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RuntimeTraceFields {
    session: SessionId,
    request: RequestId,
    operation: RuntimeOperation,
    target: String,
    admission: RuntimeAdmissionKind,
    replacement_key: Option<RuntimeAdmissionKey>,
}

impl RuntimeTraceFields {
    fn for_command(identity: RuntimeIdentity, command: &RuntimeCommand) -> Self {
        let descriptor = command.descriptor();
        Self {
            session: identity.session,
            request: identity.request,
            operation: descriptor.context.operation,
            target: runtime_target_for_trace(&descriptor.context.target),
            admission: descriptor.admission.kind,
            replacement_key: descriptor.admission.replacement_key,
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
            admission = ?self.admission,
            replacement_key = ?self.replacement_key,
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

fn runtime_target_kind(target: &RuntimeTarget) -> &'static str {
    match target {
        RuntimeTarget::Workspace => "workspace",
        RuntimeTarget::Channel(_) => "channel",
        RuntimeTarget::Thread { .. } => "thread",
        RuntimeTarget::User(_) => "user",
        RuntimeTarget::File(_) => "file",
        RuntimeTarget::Image(_) => "image",
        RuntimeTarget::Media(_) => "media",
        RuntimeTarget::Attachment(_) => "attachment",
        RuntimeTarget::ExactMessage { .. } => "exact-message",
        RuntimeTarget::Message { .. } => "message",
        RuntimeTarget::Upload { .. } => "upload",
        RuntimeTarget::Huddle(_) => "huddle",
    }
}

impl OperationContext {
    pub fn new(operation: RuntimeOperation, target: RuntimeTarget) -> Self {
        Self { operation, target }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct RuntimeCommandDescriptor {
    context: OperationContext,
    supersedes_previous: bool,
    navigation_slot: Option<NavigationSlot>,
    lane: RuntimeTaskLane,
    admission: RuntimeAdmissionPolicy,
}

impl std::fmt::Debug for RuntimeCommandDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeCommandDescriptor")
            .field("operation", &self.context.operation)
            .field("target", &runtime_target_kind(&self.context.target))
            .field("supersedes_previous", &self.supersedes_previous)
            .field("navigation_slot", &self.navigation_slot)
            .field("lane", &self.lane)
            .field("admission", &self.admission)
            .finish()
    }
}

impl RuntimeCommandDescriptor {
    fn request(
        context: OperationContext,
        lane: RuntimeTaskLane,
        admission: RuntimeAdmissionPolicy,
    ) -> Self {
        Self {
            context,
            supersedes_previous: true,
            navigation_slot: None,
            lane,
            admission,
        }
    }

    fn navigation(context: OperationContext, slot: NavigationSlot) -> Self {
        Self {
            context,
            supersedes_previous: true,
            navigation_slot: Some(slot),
            lane: RuntimeTaskLane::Navigation,
            admission: RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Navigation(slot)),
        }
    }

    fn mutation(
        context: OperationContext,
        lane: RuntimeTaskLane,
        admission: RuntimeAdmissionPolicy,
    ) -> Self {
        Self {
            context,
            supersedes_previous: false,
            navigation_slot: None,
            lane,
            admission,
        }
    }
}

fn huddle_admission_policy(command: &HuddleCommand) -> RuntimeAdmissionPolicy {
    match command {
        HuddleCommand::OpenPreflight { .. }
        | HuddleCommand::Join { .. }
        | HuddleCommand::OpenExternally { .. }
        | HuddleCommand::Leave
        | HuddleCommand::Dismiss
        | HuddleCommand::SetMuted(_)
        | HuddleCommand::SetCameraEnabled(_)
        | HuddleCommand::SetScreenShareEnabled(_)
        | HuddleCommand::SelectDevice { .. } => RuntimeAdmissionPolicy::durable_action(),
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
                RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Authentication),
            ),
            Self::StartOAuth { .. } | Self::StartBrowserSession { .. } => {
                RuntimeCommandDescriptor::request(
                    workspace(RuntimeOperation::Authenticate),
                    RuntimeTaskLane::Interactive,
                    RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Authentication),
                )
            }
            Self::SignOut => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::SignOut),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::control(),
            ),
            Self::Disconnect => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::Disconnect),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::control(),
            ),
            Self::RefreshConversations => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::Conversations),
                RuntimeTaskLane::Background,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::WorkspaceRefresh),
            ),
            Self::UpdateAttentionPreferences(_) => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::SocketMode),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::DiscoverConversations => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::ConversationDiscovery),
                RuntimeTaskLane::Background,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::ConversationDiscovery(
                    ConversationDiscoveryScope::Full,
                )),
            ),
            Self::DiscoverChannels => RuntimeCommandDescriptor::request(
                workspace(RuntimeOperation::ConversationDiscovery),
                RuntimeTaskLane::Background,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::ConversationDiscovery(
                    ConversationDiscoveryScope::Channels,
                )),
            ),
            Self::JoinConversation { channel_id } => RuntimeCommandDescriptor::request(
                channel(RuntimeOperation::OpenConversation, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::LeaveConversation { channel_id } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::LeaveConversation, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::OpenDirectMessage { user_id } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::OpenConversation,
                    RuntimeTarget::User(user_id.clone()),
                ),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::OpenGroupDirectMessage { .. } | Self::CreateChannel { .. } => {
                RuntimeCommandDescriptor::mutation(
                    workspace(RuntimeOperation::OpenConversation),
                    RuntimeTaskLane::Interactive,
                    RuntimeAdmissionPolicy::durable_action(),
                )
            }
            Self::InviteToChannel { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::OpenConversation, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
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
            Self::LoadUser { user_id } => RuntimeCommandDescriptor::request(
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::User(user_id.clone())),
                RuntimeTaskLane::Background,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                    scope: UserLoadScope::Basic,
                    target: OpaqueAdmissionTarget::digest(&[user_id]),
                }),
            ),
            Self::LoadUserProfile { user_id } => RuntimeCommandDescriptor::request(
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::User(user_id.clone())),
                RuntimeTaskLane::Background,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                    scope: UserLoadScope::Profile,
                    target: OpaqueAdmissionTarget::digest(&[user_id]),
                }),
            ),
            Self::LoadImageAsset { key, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::ImageAsset,
                    RuntimeTarget::Image(key.clone()),
                ),
                RuntimeTaskLane::Image,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::ImageAsset(
                    OpaqueAdmissionTarget::digest(&[key]),
                )),
            ),
            Self::LoadMedia { url, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(RuntimeOperation::Media, RuntimeTarget::Media(url.clone())),
                RuntimeTaskLane::Image,
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::Media(
                    OpaqueAdmissionTarget::digest(&[url]),
                )),
            ),
            Self::DownloadAttachment { url, .. } => RuntimeCommandDescriptor::request(
                OperationContext::new(
                    RuntimeOperation::AttachmentDownload,
                    RuntimeTarget::Attachment(url.clone()),
                ),
                RuntimeTaskLane::Image,
                RuntimeAdmissionPolicy::durable_action(),
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
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::MessagePermalink(
                    OpaqueAdmissionTarget::digest(&[channel_id, ts]),
                )),
            ),
            Self::ExecuteMessageAction { request, .. } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::MessageAction,
                    RuntimeTarget::ExactMessage {
                        channel_id: request.channel_id.clone(),
                        ts: request.message_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::MarkConversationRead { channel_id, .. } => RuntimeCommandDescriptor::request(
                channel(RuntimeOperation::ReadMarker, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::read_marker(),
            ),
            Self::MarkConversationReadAll { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::ReadMarker, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::read_marker(),
            ),
            Self::MarkThreadRead {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                thread(RuntimeOperation::ReadMarker, channel_id, thread_ts),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::read_marker(),
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
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::UpdateMessage {
                channel_id,
                original,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::UpdateMessage,
                    RuntimeTarget::ExactMessage {
                        channel_id: channel_id.clone(),
                        ts: original.ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
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
                RuntimeAdmissionPolicy::durable_action(),
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
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::SetConversationStarred { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::ConversationStar, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::SetCurrentUserStatus { .. } => RuntimeCommandDescriptor::mutation(
                workspace(RuntimeOperation::UserStatus),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::UploadFiles {
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
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::Huddle(command) => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::Huddle,
                    RuntimeTarget::Huddle(command.call_id().unwrap_or("active").to_string()),
                ),
                RuntimeTaskLane::Interactive,
                huddle_admission_policy(command),
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
    ConversationOpenCompleted {
        channel_id: String,
    },
    ConversationUpdateCompleted {
        channel_id: String,
    },
    ConversationStarUpdateCompleted {
        channel_id: String,
        starred: bool,
    },
    CurrentUserStatusUpdateCompleted {
        user_id: String,
        cleared: bool,
    },
    ConversationLeft {
        channel_id: String,
    },
    AttentionNotificationCandidate {
        channel_id: String,
        message: Box<SlackMessage>,
        decision: AttentionDecision,
    },
    HistoryLoadCompleted {
        channel_id: String,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
        cached: bool,
    },
    ThreadLoadCompleted {
        channel_id: String,
        thread_ts: String,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
    },
    MessageContextLoadCompleted {
        location: SearchMessageLocation,
        message_timestamps: Vec<String>,
    },
    SearchLoaded(Vec<SearchMatch>),
    FilesLoaded(Vec<SlackFile>),
    FileLoaded {
        file: Box<SlackFile>,
        share_requested: bool,
    },
    SavedItemsLoaded(Vec<SavedItem>),
    UserProfileLoadCompleted {
        user_id: String,
    },
    EmojiCatalogLoaded(HashMap<String, String>),
    ImageAssetLoaded {
        key: String,
        asset: CachedAssetDescriptor,
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
    MessageActionCompleted {
        control_handle: MessageControlHandle,
    },
    MessageActionFailed {
        control_handle: MessageControlHandle,
        failure: RuntimeFailure,
    },
    MessagePostCompleted {
        channel_id: String,
        message_ts: String,
        thread_ts: Option<String>,
    },
    MessageUpdateCompleted {
        channel_id: String,
        message_ts: String,
    },
    ReactionUpdateCompleted {
        channel_id: String,
        message_ts: String,
        thread_ts: Option<String>,
        projected: bool,
    },
    SavedUpdated {
        channel_id: String,
        message_ts: String,
        saved: bool,
        thread_ts: Option<String>,
    },
    RealtimeStatusChanged(RealtimeStatus),
    WorkspaceRefreshRequested,
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
            Self::ConversationOpenCompleted { channel_id }
            | Self::ConversationUpdateCompleted { channel_id } => OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::ConversationStarUpdateCompleted { channel_id, .. } => OperationContext::new(
                RuntimeOperation::ConversationStar,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::CurrentUserStatusUpdateCompleted { .. } => {
                OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace)
            }
            Self::ConversationLeft { channel_id } => OperationContext::new(
                RuntimeOperation::LeaveConversation,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::HistoryLoadCompleted {
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
            Self::ThreadLoadCompleted {
                channel_id,
                thread_ts,
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
                    thread_ts: thread_ts.clone(),
                },
            ),
            Self::MessageContextLoadCompleted { location, .. } => {
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
            Self::UserProfileLoadCompleted { user_id } => {
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::User(user_id.clone()))
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
            Self::RealtimeStatusChanged(_) | Self::WorkspaceRefreshRequested => {
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
            | Self::MessagePostCompleted { .. }
            | Self::MessageUpdateCompleted { .. }
            | Self::ReactionUpdateCompleted { .. }
            | Self::SavedUpdated { .. }
            | Self::MessageActionCompleted { .. }
            | Self::MessageActionFailed { .. }
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

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) enum SyncJobPayload {
    WorkspaceStartup,
    WorkspaceRefresh,
    LoadHistory { channel_id: String },
    LoadThread { channel_id: String, ts: String },
    MembershipSync { channel_id: String },
}

#[derive(Clone)]
struct RuntimeConnection {
    slack: SlackApi,
    workspace_url: Option<String>,
    workspace_store: Option<WorkspaceStore>,
    image_cache_scope: String,
    workspace: WorkspaceReducerAdapter,
    current_user_id: Option<String>,
    user_cache: Arc<Mutex<HashMap<String, String>>>,
    read_marks: Arc<Mutex<HashMap<String, String>>>,
    message_handoffs: Arc<Mutex<MessageHandoffResolver>>,
    conversation_star_sync: ConversationStarSyncGate,
    user_status_sync: UserStatusSync,
    team_id: Option<String>,
    huddles: HuddleActorHandle,
    scheduler: Arc<Mutex<SyncScheduler>>,
    pending_jobs: Arc<Mutex<HashMap<SyncJobId, SyncJobPayload>>>,
    next_job_id: Arc<std::sync::atomic::AtomicU64>,
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
    fn user_revision(&self, user_id: &str) -> u64 {
        self.state
            .lock()
            .expect("user status sync lock poisoned")
            .user_revisions
            .get(user_id)
            .copied()
            .unwrap_or_default()
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
}

#[derive(Clone, Debug, Default)]
struct WorkspaceReducerAdapter {
    coordinator: Arc<Mutex<WorkspaceCoordinator>>,
    attention_metrics: Arc<AttentionMetrics>,
    store_batch_admission: Arc<tokio::sync::Mutex<()>>,
    pending_writes: Arc<Mutex<std::collections::VecDeque<PendingWorkspaceWrite>>>,
    #[cfg(test)]
    history_completion_send_gate: Arc<Mutex<Option<Arc<TestWorkspacePatchSendGate>>>>,
}

#[derive(Clone, Debug)]
struct PendingWorkspaceWrite {
    batch: Option<StoreBatch>,
    reduction: Option<WorkspaceReduction>,
    persisted: bool,
    repair: bool,
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

    fn message(&self, channel_id: &str, message_ts: &str) -> Option<SlackMessage> {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .message(channel_id, message_ts)
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

    fn apply(
        &self,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> Option<WorkspaceReduction> {
        let reduction = self
            .coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .apply_from(origin, mutation);
        self.observe_reduction(origin, reduction.as_ref());
        reduction
    }

    fn observe_reduction(&self, origin: MutationOrigin, reduction: Option<&WorkspaceReduction>) {
        if let Some(reduction) = reduction {
            let revision = reduction.patch().revision().value();
            for effect in reduction.effects() {
                let WorkspaceEffect::MessageAttention(effect) = effect;
                self.attention_metrics.record_decision(
                    revision,
                    origin,
                    effect.delivery,
                    &effect.decision,
                );
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
    }

    #[cfg(test)]
    async fn apply_persisted(
        &self,
        store: Option<&WorkspaceStore>,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        let _admission = self.store_batch_admission.lock().await;
        self.apply_persisted_admitted(store, origin, mutation).await
    }

    /// Returns persisted reductions in revision order while the caller holds
    /// store admission. A pending failure does not prevent the current mutation
    /// from entering the ordered journal.
    async fn apply_persisted_admitted(
        &self,
        store: Option<&WorkspaceStore>,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
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
        self.enqueue_reduction(store, reduction.clone());
        Some(reduction)
    }

    fn enqueue_reduction(&self, store: Option<&WorkspaceStore>, reduction: WorkspaceReduction) {
        let batch = reduction.store_batch().cloned();
        let persisted = store.is_none() || batch.is_none();
        self.pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned")
            .push_back(PendingWorkspaceWrite {
                batch,
                reduction: Some(reduction.clone()),
                persisted,
                repair: false,
            });
    }

    /// Flushes and drains all admitted reductions in revision order. Socket
    /// messages use this before durable attention classification so an older
    /// read or unread mutation cannot make the classification stale.
    async fn recover_persisted_admitted(
        &self,
        store: Option<&WorkspaceStore>,
    ) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
        self.persist_pending_writes(store).await?;
        Ok(self.drain_persisted_admitted())
    }

    /// Drains only after the caller has completed any awaited compatibility
    /// work that must precede publication.
    fn drain_persisted_admitted(&self) -> Vec<WorkspaceReduction> {
        let mut pending = self
            .pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned");
        debug_assert!(pending.iter().all(|entry| entry.persisted));
        pending
            .drain(..)
            .filter_map(|entry| entry.reduction)
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
        let _admission = self.store_batch_admission.lock().await;
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
        let reductions = self
            .apply_persisted_admitted(store, origin, mutation)
            .await?;
        for reduction in &reductions {
            events.send_workspace_patch(reduction.patch().clone());
        }
        if let Some(completion) = completion {
            events.send_event(completion);
        }
        Ok(reductions)
    }

    async fn repair_conversation_cache_admitted(
        &self,
        store: &WorkspaceStore,
    ) -> std::result::Result<(), StoreError> {
        let recovery_generation = store.recovery_generation();
        let (revision, conversations) = {
            let coordinator = self
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            (coordinator.revision(), coordinator.conversations())
        };
        let Some(batch) = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationsRepaired(conversations)],
        ) else {
            store.mark_conversation_cache_repaired(recovery_generation);
            return Ok(());
        };

        // A full recovery projection at the current coordinator revision
        // subsumes any older admitted deltas that the cache reset removed.
        self.pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned")
            .push_front(PendingWorkspaceWrite {
                batch: Some(batch),
                reduction: None,
                persisted: false,
                repair: true,
            });
        self.persist_pending_writes(Some(store)).await?;
        self.pending_writes
            .lock()
            .expect("pending workspace writes lock poisoned")
            .retain(|entry| entry.reduction.is_some() || !entry.persisted);
        store.mark_conversation_cache_repaired(recovery_generation);
        Ok(())
    }

    async fn persist_pending_writes(
        &self,
        store: Option<&WorkspaceStore>,
    ) -> std::result::Result<(), StoreError> {
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
                    (position, batch, entry.repair)
                });
            let Some((position, batch, repair)) = next else {
                return Ok(());
            };
            let Some(store) = store else {
                return Err(StoreError::HubClosed);
            };

            // The queue entry deliberately remains installed across this await.
            // Cancellation can therefore only cause a harmless stale replay.
            if repair {
                store.execute_store_repair_batch(batch).await?;
            } else {
                store.execute_store_batch(batch).await?;
            }
            let mut pending = self
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned");
            let entry = pending
                .get_mut(position)
                .expect("persisted reduction disappeared while admission was held");
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
struct ActiveRequest {
    task_id: u64,
}

async fn wait_for_realtime_or_shutdown<F>(
    shutdown: &mut oneshot::Receiver<()>,
    future: F,
) -> Option<F::Output>
where
    F: Future,
{
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|context| {
        if std::pin::Pin::new(&mut *shutdown).poll(context).is_ready() {
            std::task::Poll::Ready(None)
        } else {
            future.as_mut().poll(context).map(Some)
        }
    })
    .await
}

struct RealtimeSessionSupervisor {
    session: SessionId,
    start: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl RealtimeSessionSupervisor {
    fn spawn<F, Fut>(session: SessionId, run: F) -> Self
    where
        F: FnOnce(oneshot::Receiver<()>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (start, start_receiver) = oneshot::channel();
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let Some(started) =
                wait_for_realtime_or_shutdown(&mut shutdown_receiver, start_receiver).await
            else {
                return;
            };
            if started.is_err() {
                return;
            }
            run(shutdown_receiver).await;
        });
        Self {
            session,
            start: Some(start),
            shutdown: Some(shutdown),
            task,
        }
    }

    fn start(&mut self) -> bool {
        self.start
            .take()
            .is_some_and(|start| start.send(()).is_ok())
    }

    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Err(error) = self.task.await {
            crate::debug::log(
                "socket",
                &format!(
                    "RealtimeSupervisorFailed session={:?} error={error}",
                    self.session
                ),
            );
        }
    }
}

struct RuntimeState {
    active_session: SessionId,
    connection: Option<RuntimeConnection>,
    realtime: Option<RealtimeSessionSupervisor>,
    attention_preferences: AttentionPreferences,
    tasks: HashMap<u64, tokio::task::AbortHandle>,
    task_requests: HashMap<u64, TrackedRequest>,
    active_requests: HashMap<OperationContext, ActiveRequest>,
    latest_requests: HashMap<OperationContext, RequestId>,
    active_navigation: HashMap<NavigationSlot, ActiveRequest>,
    latest_navigation: HashMap<NavigationSlot, RequestId>,
    next_task_id: u64,
}

impl RuntimeState {
    fn new(active_session: SessionId) -> Self {
        Self {
            active_session,
            connection: None,
            realtime: None,
            attention_preferences: AttentionPreferences::default(),
            tasks: HashMap::new(),
            task_requests: HashMap::new(),
            active_requests: HashMap::new(),
            latest_requests: HashMap::new(),
            active_navigation: HashMap::new(),
            latest_navigation: HashMap::new(),
            next_task_id: 0,
        }
    }

    fn begin_session_replacement(
        &mut self,
        session: SessionId,
    ) -> Option<RealtimeSessionSupervisor> {
        self.active_session = session;
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        self.active_requests.clear();
        self.latest_requests.clear();
        self.task_requests.clear();
        self.active_navigation.clear();
        self.latest_navigation.clear();
        self.connection = None;
        self.realtime.take()
    }

    fn install_realtime_supervisor(
        &mut self,
        mut supervisor: RealtimeSessionSupervisor,
    ) -> std::result::Result<(), RealtimeSessionSupervisor> {
        if self.active_session != supervisor.session || self.realtime.is_some() {
            return Err(supervisor);
        }
        if !supervisor.start() {
            return Err(supervisor);
        }
        self.realtime = Some(supervisor);
        Ok(())
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

    fn register_task(
        &mut self,
        session: SessionId,
        task_id: u64,
        request: Option<TrackedRequest>,
        task: tokio::task::AbortHandle,
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
                .flatten()
                .map(|active| active.task_id);
            let navigation_task = request
                .navigation_slot
                .and_then(|slot| self.active_navigation.get(&slot).copied())
                .map(|active| active.task_id);
            if let Some(previous_task_id) = context_task {
                self.abort_task(previous_task_id);
            }
            if let Some(previous_task_id) = navigation_task {
                if Some(previous_task_id) != context_task {
                    self.abort_task(previous_task_id);
                }
            }

            if request.supersedes_previous {
                self.latest_requests
                    .insert(request.context.clone(), request.identity.request);
                self.active_requests
                    .insert(request.context.clone(), ActiveRequest { task_id });
            }
            if let Some(slot) = request.navigation_slot {
                self.latest_navigation
                    .insert(slot, request.identity.request);
                self.active_navigation
                    .insert(slot, ActiveRequest { task_id });
            }
            self.task_requests.insert(task_id, request.clone());
        }

        self.tasks.insert(task_id, task);
        true
    }

    fn finish_task(&mut self, task_id: u64, request: Option<&TrackedRequest>) {
        self.tasks.remove(&task_id);
        self.task_requests.remove(&task_id);
        if let Some(request) = request {
            if request.supersedes_previous {
                let is_current = self
                    .active_requests
                    .get(&request.context)
                    .is_some_and(|active| active.task_id == task_id);
                if is_current {
                    self.active_requests.remove(&request.context);
                }
            }
            if let Some(slot) = request.navigation_slot {
                let is_current = self
                    .active_navigation
                    .get(&slot)
                    .is_some_and(|active| active.task_id == task_id);
                if is_current {
                    self.active_navigation.remove(&slot);
                }
            }
        }
    }

    fn abort_task(&mut self, task_id: u64) {
        if let Some(task) = self.tasks.remove(&task_id) {
            task.abort();
        }
        if let Some(request) = self.task_requests.remove(&task_id) {
            if request.supersedes_previous
                && self
                    .active_requests
                    .get(&request.context)
                    .is_some_and(|active| active.task_id == task_id)
            {
                self.active_requests.remove(&request.context);
            }
            if let Some(slot) = request.navigation_slot {
                if self
                    .active_navigation
                    .get(&slot)
                    .is_some_and(|active| active.task_id == task_id)
                {
                    self.active_navigation.remove(&slot);
                }
            }
        }
    }
}

async fn replace_session_and_drain(state: &Arc<Mutex<RuntimeState>>, session: SessionId) {
    let realtime = state
        .lock()
        .expect("runtime state lock poisoned")
        .begin_session_replacement(session);
    if let Some(realtime) = realtime {
        realtime.shutdown().await;
    }
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
    let parent_span = tracing::Span::current();
    let task = tokio::spawn(
        async move {
            if task_started.await.is_err() {
                return;
            }
            future.await;
            state_after_task
                .lock()
                .expect("runtime state lock poisoned")
                .finish_task(task_id, request_after_task.as_ref());
        }
        .instrument(parent_span),
    );
    let registered = state
        .lock()
        .expect("runtime state lock poisoned")
        .register_task(session, task_id, request, task.abort_handle());
    if registered {
        let _ = start_task.send(());
    }
}

#[derive(Clone, Debug)]
pub struct AppRuntime {
    commands: mpsc::UnboundedSender<RuntimeRequest>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CachedAssetDescriptor {
    workspace_key: String,
    cache_key: String,
    mime_type: PreviewAssetMime,
    size: u64,
}

impl CachedAssetDescriptor {
    pub(crate) fn new(
        workspace_key: String,
        cache_key: String,
        mime_type: PreviewAssetMime,
        size: u64,
    ) -> Option<Self> {
        let size = usize::try_from(size).ok()?;
        (valid_image_asset_cache_key(&workspace_key)
            && valid_image_asset_cache_key(&cache_key)
            && mime_type.validate_size(size))
        .then_some(Self {
            workspace_key,
            cache_key,
            mime_type,
            size: size as u64,
        })
    }

    pub(crate) fn workspace_key(&self) -> &str {
        &self.workspace_key
    }

    pub(crate) fn cache_key(&self) -> &str {
        &self.cache_key
    }

    pub(crate) fn content_type(&self) -> &'static str {
        self.mime_type.as_str()
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn is_video(&self) -> bool {
        self.mime_type.is_video()
    }

    pub(crate) fn uri(&self) -> String {
        format!("conduit-asset://{}", self.cache_key)
    }

    pub(crate) fn matches_source(&self, key: &str) -> bool {
        self.cache_key == preview_asset_cache_key(&self.workspace_key, key)
    }

    pub(crate) fn path_in(&self, root: &Path) -> PathBuf {
        root.join(&self.workspace_key).join(format!(
            "{}.{}",
            self.cache_key,
            self.mime_type.extension()
        ))
    }

    pub(crate) fn validates_opened_content(&self, size: u64, prefix: &[u8]) -> bool {
        size == self.size && self.mime_type.validate_cached_content(size, prefix)
    }
}

impl std::fmt::Debug for CachedAssetDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedAssetDescriptor")
            .field("workspace_key", &self.workspace_key)
            .field("cache_key", &self.cache_key)
            .field("mime_type", &self.mime_type)
            .field("size", &self.size)
            .finish()
    }
}

#[derive(Clone, Debug)]
struct ImageAssetCache {
    directory: PathBuf,
    operations: Arc<tokio::sync::Mutex<()>>,
}

impl ImageAssetCache {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            operations: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn load(
        &self,
        workspace_scope: &str,
        key: &str,
    ) -> Result<Option<CachedAssetDescriptor>> {
        let _operation = self.operations.lock().await;
        let workspace_key = preview_workspace_cache_key(workspace_scope);
        let cache_key = preview_asset_cache_key(&workspace_key, key);
        let mut loaded = None;

        for mime_type in PreviewAssetMime::ALL {
            let descriptor =
                CachedAssetDescriptor::new(workspace_key.clone(), cache_key.clone(), mime_type, 1)
                    .expect("validated preview MIME accepts one byte");
            let path = descriptor.path_in(&self.directory);
            let path_metadata = match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect cached preview asset {}", path.display())
                    });
                }
            };
            if !path_metadata.file_type().is_file() {
                return Err(anyhow!("cached preview asset is not a regular file"));
            }

            let mut file = tokio::fs::File::open(&path).await.with_context(|| {
                format!("failed to open cached preview asset {}", path.display())
            })?;
            let opened_metadata = file.metadata().await.with_context(|| {
                format!("failed to inspect cached preview asset {}", path.display())
            })?;
            if !opened_metadata.is_file()
                || !same_cache_file_identity(&path_metadata, &opened_metadata)
            {
                return Err(anyhow!("cached preview asset changed while opening"));
            }

            let mut prefix = [0_u8; PREVIEW_VALIDATION_PREFIX_BYTES];
            let prefix_length = file.read(&mut prefix).await.with_context(|| {
                format!("failed to validate cached preview asset {}", path.display())
            })?;
            let current_metadata = tokio::fs::symlink_metadata(&path).await.with_context(|| {
                format!(
                    "failed to revalidate cached preview asset {}",
                    path.display()
                )
            })?;
            if !current_metadata.file_type().is_file()
                || !same_cache_file_identity(&opened_metadata, &current_metadata)
                || !mime_type
                    .validate_cached_content(opened_metadata.len(), &prefix[..prefix_length])
            {
                return Err(anyhow!(
                    "cached preview asset has invalid MIME content or size"
                ));
            }
            let descriptor = CachedAssetDescriptor::new(
                workspace_key.clone(),
                cache_key.clone(),
                mime_type,
                opened_metadata.len(),
            )
            .ok_or_else(|| anyhow!("cached preview asset descriptor is invalid"))?;
            if loaded.replace((descriptor, file)).is_some() {
                return Err(anyhow!(
                    "cached preview asset has multiple MIME representations"
                ));
            }
        }

        let Some((descriptor, file)) = loaded else {
            return Ok(None);
        };
        let file = file.into_std().await;
        let now = SystemTime::now();
        let _ = file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(now)
                .set_modified(now),
        );
        Ok(Some(descriptor))
    }

    async fn store(
        &self,
        workspace_scope: &str,
        key: &str,
        asset: DownloadedPreviewAsset,
    ) -> Result<CachedAssetDescriptor> {
        self.store_with_policy(workspace_scope, key, asset, PreviewCachePolicy::default())
            .await
    }

    async fn store_with_policy(
        &self,
        workspace_scope: &str,
        key: &str,
        asset: DownloadedPreviewAsset,
        policy: PreviewCachePolicy,
    ) -> Result<CachedAssetDescriptor> {
        if !asset.mime_type.is_valid_payload(&asset.bytes) {
            return Err(anyhow!(
                "downloaded preview asset has invalid MIME content or size"
            ));
        }
        let _operation = self.operations.lock().await;
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| {
                format!(
                    "failed to create preview cache directory {}",
                    self.directory.display()
                )
            })?;

        let workspace_key = preview_workspace_cache_key(workspace_scope);
        let cache_key = preview_asset_cache_key(&workspace_key, key);
        let workspace_directory = self.directory.join(&workspace_key);
        ensure_preview_workspace_directory(&workspace_directory).await?;

        for other_mime_type in PreviewAssetMime::ALL {
            if other_mime_type == asset.mime_type {
                continue;
            }
            let other_path =
                workspace_directory.join(format!("{}.{}", cache_key, other_mime_type.extension()));
            match tokio::fs::symlink_metadata(&other_path).await {
                Ok(metadata)
                    if metadata.file_type().is_file() || metadata.file_type().is_symlink() =>
                {
                    tokio::fs::remove_file(&other_path).await.with_context(|| {
                        format!(
                            "failed to remove stale cached preview asset {}",
                            other_path.display()
                        )
                    })?;
                }
                Ok(_) => return Err(anyhow!("cached preview asset path is not a file")),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect stale cached preview asset {}",
                            other_path.display()
                        )
                    })
                }
            }
        }

        let descriptor = CachedAssetDescriptor::new(
            workspace_key,
            cache_key,
            asset.mime_type,
            asset.bytes.len() as u64,
        )
        .ok_or_else(|| anyhow!("downloaded preview asset descriptor is invalid"))?;
        let destination = descriptor.path_in(&self.directory);
        let temporary = workspace_directory.join(format!(
            ".{}-{:016x}.part",
            descriptor.cache_key,
            rand::random::<u64>()
        ));
        write_preview_asset_atomically(&destination, &temporary, &asset.bytes).await?;

        let directory = self.directory.clone();
        let protected = destination.clone();
        let cleanup = tokio::task::spawn_blocking(move || {
            prune_preview_cache(&directory, Some(&protected), policy, SystemTime::now())
        })
        .await;
        let cleanup_error = match cleanup {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(anyhow!(error).context("preview cache cleanup failed")),
            Err(error) => Some(anyhow!("preview cache cleanup task failed: {error}")),
        };
        if let Some(cleanup_error) = cleanup_error {
            match tokio::fs::remove_file(&destination).await {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(rollback_error) => {
                    return Err(anyhow!(
                        "{cleanup_error:#}; failed to roll back cached preview asset: {rollback_error}"
                    ));
                }
            }
            return Err(cleanup_error);
        }

        Ok(descriptor)
    }

    async fn maintain(&self) {
        let _operation = self.operations.lock().await;
        let directory = self.directory.clone();
        let result = tokio::task::spawn_blocking(move || {
            prune_preview_cache(
                &directory,
                None,
                PreviewCachePolicy::default(),
                SystemTime::now(),
            )
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => crate::debug::log(
                "runtime",
                &format!("PreviewCacheCleanupFailed error={error}"),
            ),
            Err(error) => crate::debug::log(
                "runtime",
                &format!("PreviewCacheCleanupTaskFailed error={error}"),
            ),
        }
    }

    #[cfg(test)]
    fn path_for_key(
        &self,
        workspace_scope: &str,
        key: &str,
        mime_type: PreviewAssetMime,
    ) -> PathBuf {
        let workspace_key = preview_workspace_cache_key(workspace_scope);
        let cache_key = preview_asset_cache_key(&workspace_key, key);
        self.directory
            .join(workspace_key)
            .join(format!("{cache_key}.{}", mime_type.extension()))
    }
}

async fn ensure_preview_workspace_directory(directory: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(directory).await {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(anyhow!("preview cache workspace path is not a directory")),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            tokio::fs::create_dir(directory).await.with_context(|| {
                format!(
                    "failed to create preview workspace cache {}",
                    directory.display()
                )
            })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect preview workspace cache {}",
                directory.display()
            )
        }),
    }
}

async fn write_preview_asset_atomically(
    destination: &Path,
    temporary: &Path,
    bytes: &[u8],
) -> Result<()> {
    let mut owns_temporary = false;
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temporary)
            .await
            .with_context(|| {
                format!(
                    "failed to create temporary preview asset {}",
                    temporary.display()
                )
            })?;
        owns_temporary = true;
        file.write_all(bytes).await.with_context(|| {
            format!(
                "failed to write temporary preview asset {}",
                temporary.display()
            )
        })?;
        file.flush().await.with_context(|| {
            format!(
                "failed to flush temporary preview asset {}",
                temporary.display()
            )
        })?;
        drop(file);
        tokio::fs::rename(temporary, destination)
            .await
            .with_context(|| {
                format!(
                    "failed to finalize cached preview asset {}",
                    destination.display()
                )
            })?;
        owns_temporary = false;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if owns_temporary {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
}

fn same_cache_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub(crate) fn image_asset_cache_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

pub(crate) fn preview_workspace_cache_key(workspace_scope: &str) -> String {
    image_asset_cache_key(workspace_scope)
}

fn preview_asset_cache_key(workspace_key: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace_key.as_bytes());
    hasher.update([0]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn valid_image_asset_cache_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy)]
struct PreviewCachePolicy {
    max_age: Duration,
    max_bytes: u64,
    max_entries: usize,
}

impl Default for PreviewCachePolicy {
    fn default() -> Self {
        Self {
            max_age: PREVIEW_CACHE_MAX_AGE,
            max_bytes: PREVIEW_CACHE_MAX_BYTES,
            max_entries: PREVIEW_CACHE_MAX_ENTRIES,
        }
    }
}

struct PreviewCacheEntry {
    path: PathBuf,
    workspace_key: String,
    cache_key: String,
    size: u64,
    last_used: SystemTime,
}

type PreviewCacheOrderKey = (SystemTime, String, String, PathBuf);

fn prune_preview_cache(
    directory: &Path,
    protected: Option<&Path>,
    policy: PreviewCachePolicy,
    now: SystemTime,
) -> std::io::Result<()> {
    let root_entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut retained = BTreeMap::new();
    let mut identities = HashMap::new();
    let mut total = 0_u64;
    let mut eviction_cutoff = None;

    for root_entry in root_entries {
        let root_entry = root_entry?;
        let workspace_path = root_entry.path();
        let metadata = match std::fs::symlink_metadata(&workspace_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            remove_preview_cache_file(&workspace_path)?;
            continue;
        }
        if !metadata.is_dir() {
            remove_preview_cache_file(&workspace_path)?;
            continue;
        }
        let Some(workspace_key) = workspace_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| valid_image_asset_cache_key(name))
            .map(ToString::to_string)
        else {
            remove_preview_cache_directory(&workspace_path)?;
            continue;
        };

        let workspace_entries = std::fs::read_dir(&workspace_path)?;
        for entry in workspace_entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.file_type().is_symlink() || metadata.is_file() {
                let Some((cache_key, mime_type)) = preview_cache_file_identity(&path) else {
                    remove_preview_cache_file(&path)?;
                    continue;
                };
                if metadata.file_type().is_symlink()
                    || !mime_type
                        .validate_size(usize::try_from(metadata.len()).unwrap_or(usize::MAX))
                {
                    remove_preview_cache_file(&path)?;
                    continue;
                }
                let last_used = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let expired = now
                    .duration_since(last_used)
                    .is_ok_and(|age| age > policy.max_age);
                let is_protected = protected.is_some_and(|protected| protected == path);
                if expired && !is_protected {
                    remove_preview_cache_file(&path)?;
                    continue;
                }
                retain_preview_cache_entry(
                    &mut retained,
                    &mut identities,
                    &mut total,
                    &mut eviction_cutoff,
                    PreviewCacheEntry {
                        path,
                        workspace_key: workspace_key.clone(),
                        cache_key,
                        size: metadata.len(),
                        last_used,
                    },
                    protected,
                    policy,
                )?;
                continue;
            }
            if metadata.is_dir() {
                remove_preview_cache_directory(&path)?;
            } else {
                remove_preview_cache_file(&path)?;
            }
        }
    }

    Ok(())
}

fn retain_preview_cache_entry(
    retained: &mut BTreeMap<PreviewCacheOrderKey, PreviewCacheEntry>,
    identities: &mut HashMap<(String, String), PreviewCacheOrderKey>,
    total: &mut u64,
    eviction_cutoff: &mut Option<PreviewCacheOrderKey>,
    entry: PreviewCacheEntry,
    protected: Option<&Path>,
    policy: PreviewCachePolicy,
) -> std::io::Result<()> {
    let identity = (entry.workspace_key.clone(), entry.cache_key.clone());
    let order = (
        entry.last_used,
        entry.workspace_key.clone(),
        entry.cache_key.clone(),
        entry.path.clone(),
    );
    let entry_is_protected = protected.is_some_and(|protected| protected == entry.path);
    if !entry_is_protected
        && eviction_cutoff
            .as_ref()
            .is_some_and(|cutoff| order <= *cutoff)
    {
        remove_preview_cache_file(&entry.path)?;
        return Ok(());
    }

    if let Some(existing_order) = identities.remove(&identity) {
        let existing = retained
            .remove(&existing_order)
            .expect("preview cache identity index must match retained entries");
        *total = total.saturating_sub(existing.size);
        let existing_is_protected = protected.is_some_and(|protected| protected == existing.path);
        if existing_is_protected || (!entry_is_protected && existing_order >= order) {
            remove_preview_cache_file(&entry.path)?;
            *total = total.saturating_add(existing.size);
            identities.insert(identity, existing_order.clone());
            retained.insert(existing_order, existing);
            return Ok(());
        }
        remove_preview_cache_file(&existing.path)?;
    }

    *total = total.saturating_add(entry.size);
    identities.insert(identity, order.clone());
    retained.insert(order, entry);

    while *total > policy.max_bytes || retained.len() > policy.max_entries {
        let Some(oldest_order) = retained
            .iter()
            .find(|(_, entry)| protected.is_none_or(|protected| protected != entry.path))
            .map(|(order, _)| order.clone())
        else {
            return Err(std::io::Error::other(
                "preview cache bounds could not be enforced",
            ));
        };
        let oldest = retained
            .remove(&oldest_order)
            .expect("selected preview cache entry must exist");
        remove_preview_cache_file(&oldest.path)?;
        *total = total.saturating_sub(oldest.size);
        identities.remove(&(oldest.workspace_key, oldest.cache_key));
        if eviction_cutoff
            .as_ref()
            .is_none_or(|cutoff| oldest_order > *cutoff)
        {
            *eviction_cutoff = Some(oldest_order);
        }
    }
    Ok(())
}

fn remove_preview_cache_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_preview_cache_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn preview_cache_file_identity(path: &Path) -> Option<(String, PreviewAssetMime)> {
    let file_name = path.file_name()?.to_str()?;
    for mime_type in PreviewAssetMime::ALL {
        let suffix = format!(".{}", mime_type.extension());
        let Some(cache_key) = file_name.strip_suffix(&suffix) else {
            continue;
        };
        if valid_image_asset_cache_key(cache_key) {
            return Some((cache_key.to_string(), mime_type));
        }
    }
    None
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

fn remove_completed_upload_files(attachments: &[UploadAttachment]) {
    for attachment in attachments {
        if attachment.remove_after_upload {
            let _ = std::fs::remove_file(&attachment.path);
        }
    }
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
    image_cache.maintain().await;
    let limits = RuntimeTaskLimits::new(
        NAVIGATION_TASK_CONCURRENCY,
        INTERACTIVE_TASK_CONCURRENCY,
        BACKGROUND_TASK_CONCURRENCY,
        IMAGE_TASK_CONCURRENCY,
        UPLOAD_TASK_CONCURRENCY,
    );

    while let Some(request) = commands.recv().await {
        let RuntimeRequest { identity, command } = request;
        let trace_fields = RuntimeTraceFields::for_command(identity, &command);
        let span = trace_fields.span();
        let _entered = span.enter();
        let active_session = state
            .lock()
            .expect("runtime state lock poisoned")
            .active_session;
        if identity.session < active_session {
            continue;
        }
        if identity.session > active_session {
            replace_session_and_drain(&state, identity.session).await;
        }

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

    replace_session_and_drain(&state, SessionId::default()).await;
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
            spawn_authentication_task(
                state,
                identity,
                events,
                limits.clone(),
                failure_context,
                async move {
                    let token = if token.should_refresh() {
                        oauth.refresh(&token).await?
                    } else {
                        token
                    };
                    authenticate_token(token).await
                },
            );
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
                limits.clone(),
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
                limits.clone(),
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
    limits: RuntimeTaskLimits,
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
                        let workspace_store_scope = workspace_store_id(&auth);
                        let image_cache_scope = preview_workspace_scope(&auth);
                        let connection = RuntimeConnection {
                            slack: api,
                            workspace_url: auth.url.clone(),
                            workspace_store: Some(WorkspaceStore::new(
                                config::state_cache_dir(),
                                &workspace_store_scope,
                            )),
                            image_cache_scope,
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
                            scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                                SchedulerConfig::new(256, 8, 5).unwrap(),
                            ))),
                            pending_jobs: Arc::new(Mutex::new(HashMap::new())),
                            next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
                        limits,
                        huddle_receiver,
                    )
                    .await;
                }
                Err(error) => {
                    let failure = authentication_failure(failure_context, &error);
                    send_lifecycle_failure_with(&events, &error, failure);
                }
            }
        },
    );
}

async fn spawn_workspace_tasks(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    limits: RuntimeTaskLimits,
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
    let hydration_limits = limits.clone();
    spawn_session_task(state, identity.session, async move {
        if let Some(store) = hydration_connection.workspace_store.as_ref() {
            if let Err(error) = store.ensure_workspace_identity().await {
                crate::debug::log(
                    "store",
                    &format!("WorkspaceIdentityStoreFailed error={error:#}"),
                );
            }
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        schedule_job_internal(
            &hydration_connection,
            SyncJobPayload::WorkspaceStartup,
            SyncJobPolicy {
                priority: SyncPriority::Interactive,
                durability: SyncDurability::Ephemeral,
                freshness: FreshnessPolicy::Always,
                replacement: ReplacementClass::Refresh(RefreshClass::Workspace),
                retry: RetryPolicy::Never,
            },
            now_ms,
        );

        let _ = hydration_ready_sender.send(());

        drive_scheduler(
            &state_after_hydration,
            identity,
            hydration_connection,
            hydration_limits,
            hydration_events,
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
            let supervisor = RealtimeSessionSupervisor::spawn(
                identity.session,
                move |mut shutdown| async move {
                    if wait_for_realtime_or_shutdown(&mut shutdown, hydration_ready_receiver)
                        .await
                        .is_none()
                    {
                        return;
                    }
                    run_socket_mode(credentials, socket_events, connection, shutdown).await;
                },
            );
            let rejected = state
                .lock()
                .expect("runtime state lock poisoned")
                .install_realtime_supervisor(supervisor)
                .err();
            if let Some(rejected) = rejected {
                rejected.shutdown().await;
            }
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

fn hash_opaque_id(id: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

fn sync_job_cancellation_id(payload: &SyncJobPayload, job_sequence: u64) -> CancellationId {
    // Navigation jobs share one slot; every other active job needs a unique identity.
    match payload {
        SyncJobPayload::LoadHistory { .. } | SyncJobPayload::LoadThread { .. } => {
            CancellationId::new(hash_opaque_id("navigation-main"))
        }
        _ => CancellationId::new(job_sequence),
    }
}

struct SyncJobPolicy {
    priority: SyncPriority,
    durability: SyncDurability,
    freshness: FreshnessPolicy,
    replacement: ReplacementClass,
    retry: RetryPolicy,
}

fn schedule_job_internal(
    connection: &RuntimeConnection,
    payload: SyncJobPayload,
    policy: SyncJobPolicy,
    now_ms: u64,
) -> Option<SyncJobId> {
    use std::sync::atomic::Ordering;
    let job_sequence = connection.next_job_id.fetch_add(1, Ordering::SeqCst);
    let job_id = SyncJobId::new(job_sequence);
    let cancellation_id = sync_job_cancellation_id(&payload, job_sequence);
    let target = match &payload {
        SyncJobPayload::WorkspaceStartup | SyncJobPayload::WorkspaceRefresh => {
            SyncTargetKey::new(SyncTargetKind::Workspace, 0)
        }
        SyncJobPayload::LoadHistory { channel_id } => {
            SyncTargetKey::new(SyncTargetKind::Conversation, hash_opaque_id(channel_id))
        }
        SyncJobPayload::LoadThread { channel_id, ts } => SyncTargetKey::new(
            SyncTargetKind::Thread,
            hash_opaque_id(&format!("{channel_id}:{ts}")),
        ),
        SyncJobPayload::MembershipSync { channel_id } => {
            if channel_id == "user_directory" {
                SyncTargetKey::new(SyncTargetKind::UserDirectory, 0)
            } else {
                SyncTargetKey::new(SyncTargetKind::Conversation, hash_opaque_id(channel_id))
            }
        }
    };

    let job = SyncJob::new(
        job_id,
        cancellation_id,
        target,
        policy.priority,
        policy.durability,
        policy.freshness,
        policy.replacement,
        policy.retry,
    )
    .unwrap();

    let admitted = {
        let mut scheduler = connection.scheduler.lock().unwrap();
        scheduler.admit(job.clone(), now_ms, None)
    };

    match admitted {
        Ok(AdmissionOutcome::Accepted { .. }) => {
            connection
                .pending_jobs
                .lock()
                .unwrap()
                .insert(job_id, payload);
            Some(job_id)
        }
        Ok(AdmissionOutcome::SkippedFresh(_)) => None,
        Err(rejection) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "SyncJobAdmissionRejected job_id={:?} reason={:?}",
                    job_id,
                    rejection.reason()
                ),
            );
            None
        }
    }
}

fn drive_scheduler(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    connection: RuntimeConnection,
    limits: RuntimeTaskLimits,
    events: RuntimeEventSender,
) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    loop {
        let dispatched = {
            let mut scheduler = connection.scheduler.lock().unwrap();
            scheduler.dispatch_next(now_ms)
        };

        let Some(dispatched_job) = dispatched else {
            break;
        };

        let job = dispatched_job.job().clone();
        let job_id = job.id();
        let run_id = dispatched_job.run();

        let payload = {
            connection
                .pending_jobs
                .lock()
                .unwrap()
                .get(&job_id)
                .cloned()
        };

        if let Some(payload) = payload {
            let limits = limits.clone();
            let connection_clone = connection.clone();
            let events_clone = events.clone();
            let state_clone = Arc::clone(state);

            let lane = match job.priority() {
                SyncPriority::Interactive => RuntimeTaskLane::Interactive,
                _ => RuntimeTaskLane::Background,
            };

            spawn_session_task(state, identity.session, async move {
                let _permit = limits.acquire(lane).await;

                let outcome = match run_job_payload(payload, &connection_clone, &events_clone).await
                {
                    Ok(()) => JobOutcome::Succeeded,
                    Err(error) => {
                        crate::debug::log(
                            "runtime",
                            &format!("SyncJobFailed job_id={:?} error={error:#}", job_id),
                        );
                        JobOutcome::PermanentFailure
                    }
                };

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let completion = {
                    let mut scheduler = connection_clone.scheduler.lock().unwrap();
                    let res = scheduler.complete(run_id, outcome, now_ms);
                    let counters = scheduler.counters();
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "SyncJobCompleted job_id={:?} outcome={:?} admitted={} queued={} running={} completed={} failed={} retried={}",
                            job_id, outcome, counters.admitted(), counters.queued_depth(), counters.running_depth(), counters.completed(), counters.failed(), counters.retried()
                        ),
                    );
                    res
                };

                match completion {
                    Ok(CompletionOutcome::Completed) => {
                        connection_clone
                            .pending_jobs
                            .lock()
                            .unwrap()
                            .remove(&job_id);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "SyncJobCompletionFailed job_id={:?} error={:?}",
                                job_id, error
                            ),
                        );
                    }
                }

                drive_scheduler(
                    &state_clone,
                    identity,
                    connection_clone,
                    limits,
                    events_clone,
                );
            });
        }
    }
}

async fn run_job_payload(
    payload: SyncJobPayload,
    connection: &RuntimeConnection,
    events: &RuntimeEventSender,
) -> Result<()> {
    match payload {
        SyncJobPayload::WorkspaceStartup => {
            load_cached_bootstrap(events, connection).await;

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            schedule_job_internal(
                connection,
                SyncJobPayload::WorkspaceRefresh,
                SyncJobPolicy {
                    priority: SyncPriority::Interactive,
                    durability: SyncDurability::Ephemeral,
                    freshness: FreshnessPolicy::Always,
                    replacement: ReplacementClass::Refresh(RefreshClass::Workspace),
                    retry: RetryPolicy::fixed(3, 1000).unwrap(),
                },
                now_ms,
            );

            let directory_empty = if let Some(store) = connection.workspace_store.as_ref() {
                store.load_user_names().await.unwrap_or_default().is_empty()
            } else {
                true
            };

            if directory_empty {
                schedule_job_internal(
                    connection,
                    SyncJobPayload::MembershipSync {
                        channel_id: "user_directory".to_string(),
                    },
                    SyncJobPolicy {
                        priority: SyncPriority::Maintenance,
                        durability: SyncDurability::Ephemeral,
                        freshness: FreshnessPolicy::Always,
                        replacement: ReplacementClass::Refresh(RefreshClass::UserDirectory),
                        retry: RetryPolicy::fixed(3, 1000).unwrap(),
                    },
                    now_ms,
                );
            }
        }
        SyncJobPayload::WorkspaceRefresh => {
            let cached_user_names = connection
                .user_cache
                .lock()
                .expect("runtime user cache lock poisoned")
                .clone();
            load_conversations_best_effort_with_api(
                events,
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
            .await?;
        }
        SyncJobPayload::LoadHistory { channel_id } => {
            let api = &connection.slack;
            let service = ConversationHistoryService::new(api, connection.workspace_store.as_ref());
            if let Some(messages) = service.load_cached(&channel_id).await? {
                if !messages.is_empty() {
                    observe_huddle_messages(
                        &connection.huddles,
                        connection.team_id.as_deref(),
                        &channel_id,
                        &messages,
                    );
                    publish_history_snapshot_with_completion(
                        events,
                        &connection.workspace_store,
                        &connection.workspace,
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
                    .await?;
                }
            }
        }
        SyncJobPayload::LoadThread { .. } => {
            // Placeholder/no-op for threads if needed
        }
        SyncJobPayload::MembershipSync { channel_id } => {
            if channel_id == "user_directory" {
                let api = &connection.slack;
                let users_base_revision = connection.workspace.revision();
                let users = api.users().await?;
                connection
                    .workspace
                    .apply_persisted_and_publish(
                        connection.workspace_store.as_ref(),
                        events,
                        MutationOrigin::WebApi,
                        WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                            users_base_revision,
                            users,
                        )),
                    )
                    .await?;
                if let Some(store) = connection.workspace_store.as_ref() {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    store
                        .store_sync_freshness(
                            "user_directory",
                            "workspace",
                            SyncFreshness {
                                refreshed_at_ms: Some(now_ms),
                                retry_count: 0,
                                retry_after_ms: None,
                            },
                        )
                        .await?;
                }
            }
        }
    }
    Ok(())
}

async fn load_cached_bootstrap(events: &RuntimeEventSender, connection: &RuntimeConnection) {
    let Some(store) = connection.workspace_store.as_ref() else {
        return;
    };
    let _admission = connection.workspace.store_batch_admission.lock().await;
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
    let users = users_from_cached_projections(
        &user_names,
        &user_full_names,
        &user_avatar_urls,
        &user_search_aliases,
        &user_statuses,
    );
    if !user_names.is_empty() {
        connection
            .user_cache
            .lock()
            .expect("runtime user cache lock poisoned")
            .extend(user_names.clone());
    }
    if !custom_emojis.is_empty() {
        events.send_event(RuntimeEventKind::EmojiCatalogLoaded(custom_emojis));
    }
    if let Err(error) = connection
        .workspace
        .apply_persisted_and_publish_admitted(
            Some(store),
            events,
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations: conversations.clone(),
                users,
                threads: thread_catalog.clone(),
                ..Default::default()
            }),
            None,
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!("WorkspaceBootstrapPublishFailed error={error:#}"),
        );
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

fn users_from_cached_projections(
    names: &HashMap<String, String>,
    full_names: &HashMap<String, String>,
    avatar_urls: &HashMap<String, String>,
    aliases: &HashMap<String, Vec<String>>,
    statuses: &HashMap<String, SlackUserStatus>,
) -> Vec<SlackUser> {
    let mut user_ids = names
        .keys()
        .chain(full_names.keys())
        .chain(avatar_urls.keys())
        .chain(aliases.keys())
        .chain(statuses.keys())
        .filter(|user_id| !user_id.trim().is_empty())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    user_ids.sort();
    user_ids
        .into_iter()
        .map(|user_id| {
            let status = statuses.get(&user_id);
            let mut normalized_aliases = aliases
                .get(&user_id)
                .into_iter()
                .flatten()
                .filter(|alias| {
                    names
                        .get(&user_id)
                        .is_none_or(|name| !alias.eq_ignore_ascii_case(name))
                        && full_names
                            .get(&user_id)
                            .is_none_or(|name| !alias.eq_ignore_ascii_case(name))
                })
                .cloned();
            SlackUser {
                id: Some(user_id.clone()),
                name: names.get(&user_id).cloned(),
                real_name: full_names.get(&user_id).cloned(),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: names.get(&user_id).cloned(),
                    display_name_normalized: normalized_aliases.next(),
                    real_name: full_names.get(&user_id).cloned(),
                    real_name_normalized: normalized_aliases.next(),
                    image_72: avatar_urls.get(&user_id).cloned(),
                    status_text: status.map(|status| status.text.clone()),
                    status_emoji: status.map(|status| status.emoji.clone()),
                    status_expiration: status.map(|status| status.expiration),
                    ..Default::default()
                }),
                ..Default::default()
            }
        })
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
        image_cache_scope: &connection.image_cache_scope,
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
    image_cache_scope: &'a str,
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
            let users_base_revision = context.workspace.revision();
            let users = api.users().await?;
            context
                .workspace
                .apply_persisted_and_publish(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                        users_base_revision,
                        users.clone(),
                    )),
                )
                .await?;
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
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: conversation.id,
                    },
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
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: conversation.id,
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
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: conversation.id,
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
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: conversation.id,
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
                    RuntimeEventKind::ConversationUpdateCompleted {
                        channel_id: conversation.id,
                    },
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
                    "HistoryLoadCompleted channel_id={channel_id} messages={} has_more={} next_cursor={}",
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
            publish_cached_thread(
                context.events,
                context.workspace_store,
                context.workspace,
                &channel_id,
                &ts,
            )
            .await;
            context.events.send_status("Loading thread");
            let base_revision = context.workspace.revision();
            let page = api.thread_replies(&channel_id, &ts).await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ThreadSnapshot {
                        channel_id: channel_id.clone(),
                        thread_ts: ts.clone(),
                        snapshot: SnapshotEnvelope::new(
                            base_revision,
                            crate::workspace_pipeline::MessagePage {
                                messages: page.messages.clone(),
                                next_cursor: page.next_cursor.clone(),
                                complete: !page.has_more && page.next_cursor.is_none(),
                            },
                        ),
                    },
                    RuntimeEventKind::ThreadLoadCompleted {
                        channel_id,
                        thread_ts: ts,
                        has_more: page.has_more,
                        next_cursor: page.next_cursor,
                        append_older: false,
                    },
                )
                .await?;
        }
        RuntimeCommand::LoadOlderThread {
            channel_id,
            ts,
            cursor,
        } => {
            let api = require_slack(context.slack)?;
            let base_revision = context.workspace.revision();
            crate::debug::log(
                "runtime",
                &format!("LoadOlderThread channel_id={channel_id} ts={ts}"),
            );
            context.events.send_status("Loading more replies");
            let page = api
                .thread_replies_page(&channel_id, &ts, Some(&cursor))
                .await?;
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ThreadSnapshot {
                        channel_id: channel_id.clone(),
                        thread_ts: ts.clone(),
                        snapshot: SnapshotEnvelope::new(
                            base_revision,
                            crate::workspace_pipeline::MessagePage {
                                messages: page.messages.clone(),
                                next_cursor: page.next_cursor.clone(),
                                complete: false,
                            },
                        ),
                    },
                    RuntimeEventKind::ThreadLoadCompleted {
                        channel_id,
                        thread_ts: ts,
                        has_more: page.has_more,
                        next_cursor: page.next_cursor,
                        append_older: true,
                    },
                )
                .await?;
        }
        RuntimeCommand::LoadMessageContext(location) => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Loading message context");
            let base_revision = context.workspace.revision();
            let page = if let Some(thread_ts) = location.thread_ts() {
                let page = api
                    .thread_replies_context(location.channel_id(), thread_ts, location.message_ts())
                    .await?;
                page
            } else {
                api.history_context(location.channel_id(), location.message_ts())
                    .await?
            };
            let message_timestamps = page
                .messages
                .iter()
                .map(|message| message.ts.clone())
                .collect();
            let snapshot = SnapshotEnvelope::new(
                base_revision,
                crate::workspace_pipeline::MessagePage {
                    messages: page.messages,
                    next_cursor: page.next_cursor,
                    complete: false,
                },
            );
            let mutation = if let Some(thread_ts) = location.thread_ts() {
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: location.channel_id().to_string(),
                    thread_ts: thread_ts.to_string(),
                    snapshot,
                }
            } else {
                WorkspaceMutation::HistorySnapshot {
                    channel_id: location.channel_id().to_string(),
                    snapshot,
                }
            };
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    mutation,
                    RuntimeEventKind::MessageContextLoadCompleted {
                        location,
                        message_timestamps,
                    },
                )
                .await?;
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
            if !context.user_cache.contains_key(&user_id) {
                let status_base_revision = context.user_status_sync.user_revision(&user_id);
                let api = require_slack(context.slack)?;
                let mut user = api.user(&user_id).await?;
                let status_is_current = context
                    .user_status_sync
                    .is_user_revision_current(&user_id, status_base_revision);
                if !status_is_current {
                    if let Some(profile) = user.profile.as_mut() {
                        profile.status_text = None;
                        profile.status_emoji = None;
                        profile.status_expiration = None;
                    }
                }
                context
                    .workspace
                    .apply_persisted_and_publish(
                        context.workspace_store.as_ref(),
                        context.events,
                        MutationOrigin::WebApi,
                        WorkspaceMutation::UserUpsert(user.clone()),
                    )
                    .await?;
                let display_name = user.display_name().unwrap_or_else(|| user_id.clone());
                context.user_cache.insert(user_id.clone(), display_name);
            }
        }
        RuntimeCommand::LoadUserProfile { user_id } => {
            let status_base_revision = context.user_status_sync.user_revision(&user_id);
            let api = require_slack(context.slack)?;
            let mut user = api.user(&user_id).await?;
            match api.user_profile(&user_id).await {
                Ok(profile) => user.profile = Some(profile),
                Err(error) => crate::debug::log(
                    "runtime",
                    &format!("UserProfileFieldsUnavailable user_id={user_id} error={error:#}"),
                ),
            }
            if !context
                .user_status_sync
                .is_user_revision_current(&user_id, status_base_revision)
            {
                if let Some(profile) = user.profile.as_mut() {
                    profile.status_text = None;
                    profile.status_emoji = None;
                    profile.status_expiration = None;
                }
            }
            context
                .workspace
                .apply_persisted_and_publish(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::UserUpsert(user),
                )
                .await?;
            context
                .events
                .send_event(RuntimeEventKind::UserProfileLoadCompleted { user_id });
        }
        RuntimeCommand::LoadImageAsset { key, url } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadImageAsset key={}", crate::debug::url_for_log(&key)),
            );
            match context
                .image_cache
                .load(context.image_cache_scope, &key)
                .await
            {
                Ok(Some(asset)) => {
                    crate::debug::log(
                        "runtime",
                        &format!("ImageAssetCacheHit key={}", crate::debug::url_for_log(&key)),
                    );
                    context
                        .events
                        .send_event(RuntimeEventKind::ImageAssetLoaded { key, asset });
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
                Ok(downloaded) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ImageAssetLoaded key={} mime_type={} bytes={}",
                            crate::debug::url_for_log(&key),
                            downloaded.mime_type.as_str(),
                            downloaded.bytes.len()
                        ),
                    );
                    match context
                        .image_cache
                        .store(context.image_cache_scope, &key, downloaded)
                        .await
                    {
                        Ok(asset) => context
                            .events
                            .send_event(RuntimeEventKind::ImageAssetLoaded { key, asset }),
                        Err(error) => {
                            crate::debug::log(
                                "runtime",
                                &format!(
                                    "ImageAssetCacheWriteFailed key={} error={error:#}",
                                    crate::debug::url_for_log(&key)
                                ),
                            );
                            context
                                .events
                                .send_event(RuntimeEventKind::ImageAssetFailed { key });
                        }
                    }
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
        RuntimeCommand::ExecuteMessageAction {
            request,
            control_handle,
        } => {
            let Some((workspace_url, team_id, api)) = context
                .workspace_url
                .zip(context.team_id)
                .zip(context.slack.as_ref())
                .map(|((workspace_url, team_id), api)| (workspace_url, team_id, api))
            else {
                crate::debug::log(
                    "runtime",
                    "MessageActionFailed method=private-slack-callback category=Validation error=Slack action session metadata is unavailable",
                );
                context
                    .events
                    .send_event(RuntimeEventKind::MessageActionFailed {
                        control_handle,
                        failure: RuntimeFailure::validation(
                            "Slack action session is unavailable. Sign in again.",
                        ),
                    });
                return Ok(());
            };
            match api
                .execute_message_action(workspace_url, team_id, &request)
                .await
            {
                Ok(()) => {
                    context
                        .events
                        .send_event(RuntimeEventKind::MessageActionCompleted { control_handle });
                }
                Err(error) => {
                    let category = error.category();
                    let full_error = anyhow::Error::new(error);
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "MessageActionFailed method=private-slack-callback category={category:?} error={full_error:#}"
                        ),
                    );
                    context
                        .events
                        .send_event(RuntimeEventKind::MessageActionFailed {
                            control_handle,
                            failure: RuntimeFailure::from_slack_category(category),
                        });
                }
            }
        }
        RuntimeCommand::MarkConversationRead { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            mark_conversation_read_best_effort(
                api,
                context.events,
                context.read_marks,
                context.workspace_store,
                context.workspace,
                ConversationReadRequest {
                    channel_id: &channel_id,
                    latest_ts: &ts,
                    mode: ConversationReadMode::ThroughVisible,
                },
            )
            .await;
        }
        RuntimeCommand::MarkConversationReadAll { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            mark_conversation_read_best_effort(
                api,
                context.events,
                context.read_marks,
                context.workspace_store,
                context.workspace,
                ConversationReadRequest {
                    channel_id: &channel_id,
                    latest_ts: &ts,
                    mode: ConversationReadMode::All,
                },
            )
            .await;
        }
        RuntimeCommand::MarkThreadRead {
            channel_id,
            thread_ts,
            ts,
        } => {
            context
                .workspace
                .apply_persisted_and_publish(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ThreadRead {
                        channel_id,
                        thread_ts,
                        last_read: ts,
                    },
                )
                .await?;
        }
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            blocks_json,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let mut message = api
                .post_message(
                    &channel_id,
                    &text,
                    blocks_json.as_deref(),
                    thread_ts.as_deref(),
                )
                .await?;
            if message.user.is_none() {
                message.user = context.current_user_id.map(str::to_string);
            }
            if message.thread_ts.is_none() {
                message.thread_ts = thread_ts.clone();
            }
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: channel_id.clone(),
                        message: message.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                    RuntimeEventKind::MessagePostCompleted {
                        channel_id,
                        message_ts: message.ts,
                        thread_ts,
                    },
                )
                .await?;
        }
        RuntimeCommand::UpdateMessage {
            channel_id,
            original,
            text,
            blocks_json,
        } => {
            let api = require_slack(context.slack)?;
            let updated = api
                .update_message(&channel_id, &original, &text, blocks_json.as_deref())
                .await?;
            let message_ts = original.ts.clone();
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageUpdated {
                        channel_id: channel_id.clone(),
                        original,
                        updated,
                    },
                    RuntimeEventKind::MessageUpdateCompleted {
                        channel_id,
                        message_ts,
                    },
                )
                .await?;
        }
        RuntimeCommand::SetReaction {
            channel_id,
            ts,
            name,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            api.set_reaction(&channel_id, &ts, &name, add).await?;
            let completion = RuntimeEventKind::ReactionUpdateCompleted {
                channel_id: channel_id.clone(),
                message_ts: ts.clone(),
                thread_ts,
                projected: context.current_user_id.is_some(),
            };
            if let Some(user_id) = context.current_user_id {
                context
                    .workspace
                    .apply_persisted_and_publish_with_completion(
                        context.workspace_store.as_ref(),
                        context.events,
                        MutationOrigin::Local,
                        WorkspaceMutation::ReactionChanged {
                            channel_id,
                            message_ts: ts,
                            name,
                            user_id: user_id.to_string(),
                            added: add,
                        },
                        completion,
                    )
                    .await?;
            } else {
                context.events.send_event(completion);
            }
        }
        RuntimeCommand::SetSaved {
            channel_id,
            ts,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            api.set_saved(&channel_id, &ts, add).await?;
            if let Some(mut message) = context.workspace.message(&channel_id, &ts) {
                message.is_starred = Some(add);
                context
                    .workspace
                    .apply_persisted_and_publish(
                        context.workspace_store.as_ref(),
                        context.events,
                        MutationOrigin::Local,
                        WorkspaceMutation::MessageChanged {
                            channel_id: channel_id.clone(),
                            message,
                            kind: MessageMutationKind::Changed,
                            origin: MutationOrigin::Local,
                        },
                    )
                    .await?;
            }
            context.events.send_event(RuntimeEventKind::SavedUpdated {
                channel_id,
                message_ts: ts,
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
            let cleared = profile.status().is_none();
            context.user_status_sync.publish_change(&user_id, || {});
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::Local,
                    WorkspaceMutation::UserUpsert(SlackUser {
                        id: Some(user_id.clone()),
                        profile: Some(profile),
                        ..Default::default()
                    }),
                    RuntimeEventKind::CurrentUserStatusUpdateCompleted { user_id, cleared },
                )
                .await?;
        }
        RuntimeCommand::UploadFiles {
            channel_id,
            thread_ts,
            attachments,
            blocks_json,
        } => {
            let api = require_slack(context.slack)?;
            context
                .events
                .send_event(RuntimeEventKind::FileUploadProgress {
                    fraction: 0.05,
                    label: "Preparing upload".to_string(),
                });
            let progress_events = context.events.clone();
            let paths = attachments
                .iter()
                .map(|attachment| attachment.path.clone())
                .collect::<Vec<_>>();
            let upload = api
                .upload_files(
                    &channel_id,
                    thread_ts.as_deref(),
                    &paths,
                    blocks_json.as_deref(),
                    move |update| {
                        progress_events.send_event(RuntimeEventKind::FileUploadProgress {
                            fraction: update.fraction,
                            label: update.label,
                        });
                    },
                )
                .await;
            let files = upload?;
            remove_completed_upload_files(&attachments);
            let label = if files.len() == 1 {
                let mut files = files;
                let file = files.remove(0);
                file.title
                    .or(file.name)
                    .or(file.id)
                    .unwrap_or_else(|| "file".to_string())
            } else {
                format!("{} files", files.len())
            };
            context
                .events
                .send_event(RuntimeEventKind::FileUploaded(label));
        }
    }

    Ok(())
}

async fn run_socket_mode(
    credentials: socket_mode::SocketModeCredentials,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    mut shutdown: oneshot::Receiver<()>,
) {
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
        if realtime_shutdown_requested(&mut shutdown) {
            return;
        }
        let events_for_run = events.clone();
        let connected_events = events.clone();
        let mut persistence_tasks = tokio::task::JoinSet::new();
        let persistence_sender = workspace_store.clone().map(|store| {
            let (sender, receiver) =
                realtime_persistence_channel(workspace.attention_metrics_handle());
            persistence_tasks.spawn(persist_realtime_events(
                receiver,
                store,
                current_user_id.clone(),
                events_for_run.clone(),
                workspace.clone(),
                user_status_sync.clone(),
            ));
            sender
        });
        let persistence_for_run = persistence_sender.clone();
        let workspace_for_run = workspace.clone();
        let huddles_for_run = huddles.clone();
        let team_id_for_run = team_id.clone();
        let user_status_sync_for_run = user_status_sync.clone();
        let result = {
            let run_once = socket_mode::run_once(
                &credentials,
                move || {
                    connected_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                        RealtimeStatus::online(transport),
                    ));
                },
                move |event| {
                    let persistence_for_event = persistence_for_run.clone();
                    let workspace_for_event = workspace_for_run.clone();
                    let events_for_event = events_for_run.clone();
                    let huddles_for_event = huddles_for_run.clone();
                    let team_id_for_event = team_id_for_run.clone();
                    let user_status_sync_for_event = user_status_sync_for_run.clone();
                    async move {
                        observe_huddle_socket_event(
                            &huddles_for_event,
                            team_id_for_event.as_deref(),
                            &event,
                        );
                        let defer_workspace_mutation = persistence_for_event.is_some()
                            && matches!(
                                &event,
                                SocketModeEvent::Message(_)
                                    | SocketModeEvent::Reaction(_)
                                    | SocketModeEvent::UserChanged(_)
                                    | SocketModeEvent::UserHuddleChanged(_)
                            );
                        let attention = (!defer_workspace_mutation)
                            .then(|| {
                                apply_realtime_workspace_event_and_publish(
                                    &workspace_for_event,
                                    &events_for_event,
                                    &event,
                                )
                            })
                            .flatten();
                        if matches!(&event, SocketModeEvent::RefreshConversations) {
                            events_for_event
                                .send_event(RuntimeEventKind::WorkspaceRefreshRequested);
                        }
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
                        let status_revision = status_change_user_id.as_deref().map(|user_id| {
                            user_status_sync_for_event.publish_change(user_id, || {})
                        });
                        let persistence_event = match &event {
                            SocketModeEvent::UserChanged(user)
                            | SocketModeEvent::UserHuddleChanged(user) => {
                                Some(RealtimePersistenceEvent::UserChanged {
                                    user: user.clone(),
                                    status_revision,
                                })
                            }
                            SocketModeEvent::Message(message) => {
                                Some(RealtimePersistenceEvent::Message {
                                    event: message.clone(),
                                })
                            }
                            SocketModeEvent::Reaction(_) => {
                                Some(RealtimePersistenceEvent::OrderedEvent {
                                    event: event.clone(),
                                })
                            }
                            SocketModeEvent::RefreshConversations => None,
                        };
                        let notification_without_store =
                            persistence_for_event.is_none().then(|| {
                                attention
                                    .as_ref()
                                    .filter(|effect| effect.decision.send_notification)
                                    .map(|effect| {
                                        (
                                            effect.channel_id.clone(),
                                            effect.message.clone(),
                                            effect.decision.clone(),
                                        )
                                    })
                            });
                        if let Some(sender) = persistence_for_event.as_ref() {
                            if let Some(persistence_event) = persistence_event {
                                if sender.send(persistence_event).await.is_err() {
                                    crate::debug::log(
                                        "store",
                                        "RealtimePersistenceQueueRejected reason=worker_closed",
                                    );
                                    if defer_workspace_mutation {
                                        apply_realtime_persistence_queue_fallback(
                                            &workspace_for_event,
                                            &events_for_event,
                                            event,
                                        );
                                    }
                                    return Err(anyhow!("realtime persistence worker closed"));
                                }
                            }
                        } else if let Some(Some((channel_id, message, decision))) =
                            notification_without_store
                        {
                            events_for_event.send_event(
                                RuntimeEventKind::AttentionNotificationCandidate {
                                    channel_id,
                                    message: Box::new(message),
                                    decision,
                                },
                            );
                        }
                        Ok(())
                    }
                },
            );
            wait_for_realtime_or_shutdown(&mut shutdown, run_once).await
        };
        if result.is_some() && !realtime_shutdown_requested(&mut shutdown) {
            events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                RealtimeStatus::reconnecting(transport),
            ));
        }
        drain_realtime_persistence(persistence_sender, &mut persistence_tasks, &workspace).await;

        let Some(result) = result else {
            return;
        };
        if realtime_shutdown_requested(&mut shutdown) {
            return;
        }

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
        if wait_for_realtime_or_shutdown(&mut shutdown, tokio::time::sleep(timing.sleep))
            .await
            .is_none()
        {
            return;
        }
    }
}

async fn drain_realtime_persistence(
    persistence_sender: Option<RealtimePersistenceSender>,
    persistence_tasks: &mut tokio::task::JoinSet<()>,
    workspace: &WorkspaceReducerAdapter,
) {
    drop(persistence_sender);
    while let Some(join_result) = persistence_tasks.join_next().await {
        if let Err(error) = join_result {
            crate::debug::log(
                "store",
                &format!("RealtimePersistenceWorkerFailed error={error}"),
            );
        }
    }
    workspace.trace_attention_metrics_snapshot();
}

fn realtime_shutdown_requested(shutdown: &mut oneshot::Receiver<()>) -> bool {
    match shutdown.try_recv() {
        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => true,
        Err(oneshot::error::TryRecvError::Empty) => false,
    }
}

fn apply_realtime_persistence_queue_fallback(
    workspace: &WorkspaceReducerAdapter,
    events: &RuntimeEventSender,
    event: SocketModeEvent,
) {
    if let Some(attention) = apply_realtime_workspace_event_and_publish(workspace, events, &event) {
        if attention.decision.send_notification {
            events.send_event(RuntimeEventKind::AttentionNotificationCandidate {
                channel_id: attention.channel_id,
                message: Box::new(attention.message),
                decision: attention.decision,
            });
        }
    }
}

#[cfg(test)]
fn apply_realtime_workspace_event(
    workspace: &WorkspaceReducerAdapter,
    event: &SocketModeEvent,
) -> Option<MessageAttentionEffect> {
    let reduction = reduce_realtime_workspace_event(workspace, event)?;
    reduction_message_attention(&reduction)
}

fn apply_realtime_workspace_event_and_publish(
    workspace: &WorkspaceReducerAdapter,
    events: &RuntimeEventSender,
    event: &SocketModeEvent,
) -> Option<MessageAttentionEffect> {
    let reduction = reduce_realtime_workspace_event(workspace, event)?;
    let attention = reduction_message_attention(&reduction);
    events.send_workspace_patch(reduction.patch().clone());
    attention
}

fn reduce_realtime_workspace_event(
    workspace: &WorkspaceReducerAdapter,
    event: &SocketModeEvent,
) -> Option<WorkspaceReduction> {
    workspace.apply(
        MutationOrigin::Realtime,
        realtime_workspace_mutation(event)?,
    )
}

fn realtime_workspace_mutation(event: &SocketModeEvent) -> Option<WorkspaceMutation> {
    match event {
        SocketModeEvent::Message(event) => Some(realtime_message_mutation(event, None)),
        SocketModeEvent::UserChanged(user) | SocketModeEvent::UserHuddleChanged(user) => {
            Some(WorkspaceMutation::UserUpsert((**user).clone()))
        }
        SocketModeEvent::Reaction(reaction) => Some(WorkspaceMutation::ReactionChanged {
            channel_id: reaction.channel_id.clone(),
            message_ts: reaction.ts.clone(),
            name: reaction.name.clone(),
            user_id: reaction.user_id.clone(),
            added: reaction.added,
        }),
        SocketModeEvent::RefreshConversations => None,
    }
}

fn reduction_message_attention(reduction: &WorkspaceReduction) -> Option<MessageAttentionEffect> {
    reduction
        .effects()
        .iter()
        .map(|effect| match effect {
            WorkspaceEffect::MessageAttention(effect) => effect.clone(),
        })
        .next()
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
        | SocketModeEvent::RefreshConversations => Ok(()),
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
    sender: mpsc::Sender<QueuedRealtimePersistenceEvent>,
    admission: Arc<Semaphore>,
    metrics: Arc<AttentionMetrics>,
}

impl RealtimePersistenceSender {
    async fn send(
        &self,
        event: RealtimePersistenceEvent,
    ) -> Result<(), mpsc::error::SendError<RealtimePersistenceEvent>> {
        let slot = match Arc::clone(&self.admission).acquire_owned().await {
            Ok(slot) => slot,
            Err(_) => {
                return self
                    .metrics
                    .record_queue_send(|| Err(mpsc::error::SendError(event)));
            }
        };
        match self.sender.reserve().await {
            Ok(permit) => {
                self.metrics
                    .record_queue_send(|| {
                        permit.send(QueuedRealtimePersistenceEvent { event, slot });
                        Ok::<(), std::convert::Infallible>(())
                    })
                    .expect("infallible bounded realtime persistence send");
                Ok(())
            }
            Err(_) => {
                drop(slot);
                self.metrics
                    .record_queue_send(|| Err(mpsc::error::SendError(event)))
            }
        }
    }
}

struct QueuedRealtimePersistenceEvent {
    event: RealtimePersistenceEvent,
    slot: OwnedSemaphorePermit,
}

struct RealtimePersistenceReceiver {
    receiver: mpsc::Receiver<QueuedRealtimePersistenceEvent>,
    metrics: Arc<AttentionMetrics>,
}

impl RealtimePersistenceReceiver {
    async fn recv(&mut self) -> Option<RealtimePersistenceEvent> {
        let queued = self.receiver.recv().await?;
        self.metrics.dequeue_queue_slot();
        let QueuedRealtimePersistenceEvent { event, slot } = queued;
        drop(slot);
        Some(event)
    }
}

fn realtime_persistence_channel(
    metrics: Arc<AttentionMetrics>,
) -> (RealtimePersistenceSender, RealtimePersistenceReceiver) {
    realtime_persistence_channel_with_capacity(metrics, REALTIME_PERSISTENCE_QUEUE_CAPACITY)
}

fn realtime_persistence_channel_with_capacity(
    metrics: Arc<AttentionMetrics>,
    capacity: usize,
) -> (RealtimePersistenceSender, RealtimePersistenceReceiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    let admission = Arc::new(Semaphore::new(capacity));
    (
        RealtimePersistenceSender {
            sender,
            admission,
            metrics: Arc::clone(&metrics),
        },
        RealtimePersistenceReceiver { receiver, metrics },
    )
}

struct RealtimeAttentionPersistence {
    attention_status: AttentionPersistenceStatus,
    notification_claimed: bool,
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

async fn persist_realtime_events(
    mut receiver: RealtimePersistenceReceiver,
    store: WorkspaceStore,
    current_user_id: Option<String>,
    events: RuntimeEventSender,
    workspace: WorkspaceReducerAdapter,
    user_status_sync: UserStatusSync,
) {
    while let Some(event) = receiver.recv().await {
        match event {
            RealtimePersistenceEvent::UserChanged {
                mut user,
                status_revision,
            } => {
                let Some(user_id) = user.id.clone() else {
                    continue;
                };
                let includes_status = user
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.contains_status_fields());
                let _persistence_guard = if includes_status {
                    Some(user_status_sync.persistence.lock().await)
                } else {
                    None
                };
                let status_is_current = status_revision.is_none_or(|revision| {
                    user_status_sync.is_user_revision_current(&user_id, revision)
                });
                if !status_is_current {
                    if let Some(profile) = user.profile.as_mut() {
                        profile.status_text = None;
                        profile.status_emoji = None;
                        profile.status_expiration = None;
                    }
                }
                if let Err(error) = workspace
                    .apply_persisted_and_publish_inner(
                        Some(&store),
                        &events,
                        MutationOrigin::Realtime,
                        WorkspaceMutation::UserUpsert(*user),
                        None,
                    )
                    .await
                {
                    crate::debug::log(
                        "store",
                        &format!(
                            "RealtimeUserMutationDeferred user_id={user_id} category={:?}",
                            error.category()
                        ),
                    );
                }
            }
            RealtimePersistenceEvent::Message { event } => {
                persist_socket_message(
                    &store,
                    current_user_id.as_deref(),
                    &events,
                    &workspace,
                    *event,
                )
                .await;
            }
            RealtimePersistenceEvent::OrderedEvent { event } => {
                if let Some(mutation) = realtime_workspace_mutation(&event) {
                    if let Err(error) = workspace
                        .apply_persisted_and_publish_inner(
                            Some(&store),
                            &events,
                            MutationOrigin::Realtime,
                            mutation,
                            None,
                        )
                        .await
                    {
                        crate::debug::log(
                            "store",
                            &format!(
                                "RealtimeWorkspaceMutationDeferred category={:?}",
                                error.category()
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// Serializes socket message authority with interactive history snapshots.
///
/// Older recovered patches publish first. The current typed patch then carries
/// the complete timeline and first-unread projection to the window.
async fn persist_socket_message(
    store: &WorkspaceStore,
    current_user_id: Option<&str>,
    events: &RuntimeEventSender,
    workspace: &WorkspaceReducerAdapter,
    message_event: crate::socket_mode::SocketModeMessageEvent,
) {
    let _admission = workspace.store_batch_admission.lock().await;

    let recovered = match workspace.recover_persisted_admitted(Some(store)).await {
        Ok(reductions) => reductions,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeWorkspaceRecoveryDeferred category={:?}",
                    error.category()
                ),
            );
            let preview = preview_realtime_workspace_attention(workspace, &message_event);
            let attention_failed = message_event.kind == SocketModeMessageKind::Posted
                && message_event.message.user.as_deref() != current_user_id
                && preview.is_some();
            let attention_status = if attention_failed {
                AttentionPersistenceStatus::Failed
            } else {
                AttentionPersistenceStatus::NotApplicable
            };
            workspace.record_attention_persistence(attention_status.metrics_outcome(), false);
            let current = workspace.apply_and_enqueue(
                Some(store),
                MutationOrigin::Realtime,
                realtime_message_mutation(&message_event, None),
            );
            if let Some(reduction) = current {
                events.send_workspace_patch(reduction.patch().clone());
            }
            return;
        }
    };
    for reduction in recovered {
        events.send_workspace_patch(reduction.patch().clone());
    }

    let attention = preview_realtime_workspace_attention(workspace, &message_event)
        .map(|effect| effect.decision);
    let attention_persistence =
        persist_socket_attention(store, current_user_id, &message_event, attention).await;
    workspace.record_attention_persistence(
        attention_persistence.attention_status.metrics_outcome(),
        attention_persistence.notification_claimed,
    );
    let delivery = match attention_persistence.attention_status {
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
        reduction
            .effects()
            .iter()
            .map(|effect| match effect {
                WorkspaceEffect::MessageAttention(effect) => effect.clone(),
            })
            .next()
    });
    let notification = claimed_notification_candidate(
        attention_persistence.notification_claimed,
        applied_attention.as_ref(),
    );
    let current_persisted = match workspace.persist_pending_writes(Some(store)).await {
        Ok(()) => true,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "RealtimeMessageDeltaDeferred channel_id={} category={:?}",
                    message_event.channel_id,
                    error.category()
                ),
            );
            false
        }
    };

    if current_persisted {
        for reduction in workspace.drain_persisted_admitted() {
            events.send_workspace_patch(reduction.patch().clone());
        }
    } else if let Some(current) = current.as_ref() {
        events.send_workspace_patch(current.patch().clone());
    }
    if let Some(notification) = notification {
        events.send_event(RuntimeEventKind::AttentionNotificationCandidate {
            channel_id: notification.channel_id,
            message: Box::new(notification.message),
            decision: notification.decision,
        });
    }
}

async fn persist_socket_attention(
    store: &WorkspaceStore,
    current_user_id: Option<&str>,
    message_event: &crate::socket_mode::SocketModeMessageEvent,
    attention: Option<AttentionDecision>,
) -> RealtimeAttentionPersistence {
    if message_event.kind != SocketModeMessageKind::Posted
        || message_event.message.user.as_deref() == current_user_id
    {
        return RealtimeAttentionPersistence {
            attention_status: AttentionPersistenceStatus::NotApplicable,
            notification_claimed: false,
        };
    }
    let Some(decision) = attention else {
        return RealtimeAttentionPersistence {
            attention_status: AttentionPersistenceStatus::NotApplicable,
            notification_claimed: false,
        };
    };
    let channel_id = &message_event.channel_id;
    let message = &message_event.message;
    match store
        .accept_attention_delivery_for_message(
            channel_id,
            &message.ts,
            message.thread_root_ts(),
            decision.record_unread,
            decision.send_notification,
        )
        .await
    {
        Ok(outcome) => match outcome.observation {
            AttentionObservationStatus::Accepted => RealtimeAttentionPersistence {
                attention_status: AttentionPersistenceStatus::Accepted,
                notification_claimed: outcome.notification_claimed,
            },
            AttentionObservationStatus::AlreadyObserved => {
                crate::debug::log(
                    "attention",
                    "AttentionRealtimeSuppressed reason=already_observed",
                );
                RealtimeAttentionPersistence {
                    attention_status: AttentionPersistenceStatus::AlreadyObserved,
                    notification_claimed: false,
                }
            }
            AttentionObservationStatus::AtOrBeforeReadCursor => {
                crate::debug::log(
                    "attention",
                    "AttentionRealtimeSuppressed reason=at_or_before_read_cursor",
                );
                RealtimeAttentionPersistence {
                    attention_status: AttentionPersistenceStatus::AtOrBeforeReadCursor,
                    notification_claimed: false,
                }
            }
            AttentionObservationStatus::InvalidIdentity => {
                // Coordinator preview rejects malformed identities before
                // persistence. Keep this defensive store result outside the
                // ledger-outcome counters because no ledger attempt occurred.
                crate::debug::log(
                    "attention",
                    "AttentionRealtimeSuppressed reason=invalid_identity",
                );
                RealtimeAttentionPersistence {
                    attention_status: AttentionPersistenceStatus::NotApplicable,
                    notification_claimed: false,
                }
            }
        },
        Err(error) => {
            crate::debug::log(
                "store",
                &format!(
                    "ConversationRealtimeStoreFailed channel_id={channel_id} category={:?}",
                    error.category()
                ),
            );
            RealtimeAttentionPersistence {
                attention_status: AttentionPersistenceStatus::Failed,
                notification_claimed: false,
            }
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
            RuntimeEventKind::ConversationStarUpdateCompleted {
                channel_id,
                starred,
            },
        )
        .await?;
    Ok(())
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

    let _admission = workspace.store_batch_admission.lock().await;
    if let Some(store) = workspace_store {
        store.validate_conversation_cache().await?;
        if store.conversation_cache_needs_repair() {
            workspace.repair_conversation_cache_admitted(store).await?;
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
                workspace.reducer,
                &refreshed_conversations,
                &cached_user_names,
            )
            .await;
        }
        Err(error) => handle_conversations_load_error(events, error),
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
) -> std::result::Result<Vec<WorkspaceReduction>, StoreError> {
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
                        next_cursor: None,
                        complete: true,
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
    workspace: &WorkspaceReducerAdapter,
    conversations: &[SlackConversation],
    cached_user_names: &HashMap<String, String>,
) {
    let user_ids = cached_conversation_user_ids(conversations, cached_user_names);
    if user_ids.is_empty() {
        return;
    }

    let base_revision = workspace.revision();
    let mut refreshed_users = Vec::new();
    for user_id in user_ids {
        match api.user(&user_id).await {
            Ok(user) => {
                refreshed_users.push(user);
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("UserNameRefreshFailed user_id={user_id} error={error:#}"),
            ),
        }
    }

    if refreshed_users.is_empty() {
        return;
    }

    if let Err(error) = workspace
        .apply_persisted_and_publish(
            workspace_store.as_ref(),
            events,
            MutationOrigin::WebApi,
            WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(base_revision, refreshed_users)),
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!("ConversationUserProjectionStoreFailed error={error:#}"),
        );
    }
}

fn handle_conversations_load_error(events: &RuntimeEventSender, error: anyhow::Error) {
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoadFailed error={error:#}"),
    );
    let failure = RuntimeFailure::from_error(&error);
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
    let _admission = workspace.store_batch_admission.lock().await;
    let reductions = workspace
        .apply_persisted_and_publish_admitted(
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
    #[cfg(test)]
    workspace.wait_before_history_completion();
    events.send_event(RuntimeEventKind::HistoryLoadCompleted {
        channel_id: channel_id.to_string(),
        has_more,
        next_cursor,
        append_older,
        cached,
    });
    Ok(reductions)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConversationReadMode {
    ThroughVisible,
    All,
}

#[derive(Clone, Copy, Debug)]
struct ConversationReadRequest<'a> {
    channel_id: &'a str,
    latest_ts: &'a str,
    mode: ConversationReadMode,
}

async fn mark_conversation_read_best_effort(
    api: &SlackApi,
    events: &RuntimeEventSender,
    read_marks: &mut HashMap<String, String>,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    request: ConversationReadRequest<'_>,
) {
    let ConversationReadRequest {
        channel_id,
        latest_ts,
        mode,
    } = request;
    if channel_id.trim().is_empty() || latest_ts.trim().is_empty() {
        return;
    }

    if read_marks.get(channel_id).is_some_and(|marked_ts| {
        marked_ts == latest_ts || slack_timestamp_is_after(marked_ts, latest_ts)
    }) {
        publish_local_read_marker(
            events,
            workspace_store,
            workspace,
            channel_id,
            latest_ts,
            mode,
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
    publish_local_read_marker(
        events,
        workspace_store,
        workspace,
        channel_id,
        latest_ts,
        mode,
    )
    .await;
}

async fn publish_local_read_marker(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
    channel_id: &str,
    latest_ts: &str,
    mode: ConversationReadMode,
) {
    let mutation = match mode {
        ConversationReadMode::ThroughVisible => WorkspaceMutation::ReadAdvanced {
            channel_id: channel_id.to_string(),
            ts: latest_ts.to_string(),
            remaining_unread: 0,
        },
        ConversationReadMode::All => WorkspaceMutation::ConversationReadAll {
            channel_id: channel_id.to_string(),
            ts: latest_ts.to_string(),
        },
    };
    if let Err(error) = workspace
        .apply_persisted_and_publish(
            workspace_store.as_ref(),
            events,
            MutationOrigin::Local,
            mutation,
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!(
                "ConversationReadStoreDeferred channel_id={channel_id} category={:?}",
                error.category()
            ),
        );
    }
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

pub(crate) fn preview_workspace_scope(auth: &AuthInfo) -> String {
    let workspace = [
        auth.team_id.as_deref(),
        auth.url.as_deref(),
        auth.team.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .unwrap_or("unknown-team");
    let user = [auth.user_id.as_deref(), auth.user.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("unknown-user");
    format!("{workspace}:{user}")
}

async fn publish_cached_thread(
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
                    "CachedThreadLoadCompleted channel_id={channel_id} ts={thread_ts} messages={}",
                    messages.len()
                ),
            );
            if let Err(error) = workspace
                .apply_persisted_and_publish_with_completion(
                    workspace_store.as_ref(),
                    events,
                    MutationOrigin::Cache,
                    WorkspaceMutation::ThreadSnapshot {
                        channel_id: channel_id.to_string(),
                        thread_ts: thread_ts.to_string(),
                        snapshot: SnapshotEnvelope::new(
                            WorkspaceRevision::INITIAL,
                            crate::workspace_pipeline::MessagePage {
                                messages,
                                next_cursor: None,
                                complete: true,
                            },
                        ),
                    },
                    RuntimeEventKind::ThreadLoadCompleted {
                        channel_id: channel_id.to_string(),
                        thread_ts: thread_ts.to_string(),
                        has_more: false,
                        next_cursor: None,
                        append_older: false,
                    },
                )
                .await
            {
                crate::debug::log(
                    "store",
                    &format!(
                        "CachedThreadPublishDeferred channel_id={channel_id} category={:?}",
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
    use crate::workspace_pipeline::{MessageChange, StoreChange, WorkspaceChange};

    fn timeline_patch_summary(changes: &[WorkspaceChange]) -> Vec<(String, Option<String>)> {
        changes
            .iter()
            .flat_map(|change| match change {
                WorkspaceChange::TimelineChanged { changes, .. } => changes
                    .iter()
                    .map(|change| match change {
                        MessageChange::Upsert(message) => {
                            (message.ts.clone(), Some(message.body_text()))
                        }
                        MessageChange::Remove { message_ts } => (message_ts.clone(), None),
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
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

            let stored = store.load_conversations().await.unwrap().unwrap();
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
                .await
                .unwrap();
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
            store.store_thread_catalog(&thread_records).await.unwrap();

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
                image_cache_scope: "test-workspace".to_string(),
                workspace: workspace.clone(),
                current_user_id: None,
                user_cache: Arc::new(Mutex::new(HashMap::new())),
                read_marks: Arc::new(Mutex::new(HashMap::new())),
                message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                conversation_star_sync: ConversationStarSyncGate::default(),
                user_status_sync: UserStatusSync::default(),
                team_id: None,
                huddles,
                scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                    SchedulerConfig::new(256, 8, 5).unwrap(),
                ))),
                pending_jobs: Arc::new(Mutex::new(HashMap::new())),
                next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cached_bootstrap_load_gate: None,
            };

            load_cached_bootstrap(&events, &connection).await;

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(
                delivered
                    .iter()
                    .map(|event| match &event.kind {
                        RuntimeEventKind::WorkspacePatch(_) => "patch",
                        RuntimeEventKind::EmojiCatalogLoaded(_) => "emoji",
                        other => panic!("unexpected cache projection event {other:?}"),
                    })
                    .collect::<Vec<_>>(),
                vec!["emoji", "patch"]
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
                          && data.users.iter().any(|user| {
                              user.id.as_deref() == Some("U1")
                                  && user.display_name().as_deref() == Some("Ada")
                          })
                          && data.threads == thread_records
            ));
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
                    store
                        .store_conversations(std::slice::from_ref(&cached))
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
                    image_cache_scope: "test-workspace".to_string(),
                    workspace: workspace.clone(),
                    current_user_id: None,
                    user_cache: Arc::new(Mutex::new(HashMap::new())),
                    read_marks: Arc::new(Mutex::new(HashMap::new())),
                    message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                    conversation_star_sync: ConversationStarSyncGate::default(),
                    user_status_sync: UserStatusSync::default(),
                    team_id: None,
                    huddles,
                    scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                        SchedulerConfig::new(256, 8, 5).unwrap(),
                    ))),
                    pending_jobs: Arc::new(Mutex::new(HashMap::new())),
                    next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
                    Some(RuntimeEventKind::WorkspacePatch(_))
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
                    vec![1, 2]
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

            let stored = store.load_conversations().await.unwrap().unwrap();
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
    fn recovered_refresh_metadata_cannot_roll_back_a_canonical_local_read() {
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
            publish_local_read_marker(
                &events,
                &workspace_store,
                &workspace,
                "C1",
                "20.0",
                ConversationReadMode::All,
            )
            .await;
            view.conversations
                .borrow_mut()
                .advance_read_cursor("C1", "20.0", 0);
            let local_reads = HashMap::from([("C1".to_string(), "20.0".to_string())]);
            let before_recovery = store.load_conversations().await.unwrap().unwrap();
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

            let recovered = receiver.recv().await.unwrap();
            let read = receiver.recv().await.unwrap();
            let following = receiver.recv().await.unwrap();
            let patches = [recovered, read, following]
                .into_iter()
                .map(|event| match event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch,
                    other => panic!("expected a typed workspace patch, got {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(patches[0].changes().len(), 2);
            assert!(matches!(
                patches[0].changes(),
                [
                    WorkspaceChange::ConversationMetadataUpsert(metadata),
                    WorkspaceChange::UnreadChanged { snapshot },
                ] if metadata.name.as_deref() == Some("one refreshed")
                    && snapshot.channel_id == "C1"
            ));
            let recovered_application = view
                .apply_workspace_patch_with_local_reads(&patches[0], &local_reads)
                .unwrap();
            assert!(
                recovered_application.conversation_changed(),
                "ordinary refreshed metadata should still render"
            );
            for patch in &patches[1..] {
                view.apply_workspace_patch_with_local_reads(patch, &local_reads)
                    .unwrap();
            }
            assert!(receiver.try_recv().is_err());

            {
                let conversations = view.conversations.borrow();
                let current = conversations.get("C1").unwrap();
                assert_eq!(current.name.as_deref(), Some("one refreshed"));
                assert_eq!(current.unread_activity_count(), 0);
                assert_eq!(current.last_read_ts(), Some("20.0"));
                assert_eq!(current.latest_message_ts(), Some("19.0"));
            }
            let stored = store.load_conversations().await.unwrap().unwrap();
            let stored = stored
                .iter()
                .find(|conversation| conversation.id == "C1")
                .unwrap();
            assert_eq!(stored.name.as_deref(), Some("one refreshed"));
            assert_eq!(stored.unread_activity_count(), 0);
            assert_eq!(stored.last_read_ts(), Some("20.0"));
            assert_eq!(stored.local_read_ts(), Some("20.0"));
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
        });
        let _ = std::fs::remove_dir_all(directory);
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
                assert!(matches!(
                    receiver.recv().await.unwrap().kind,
                    RuntimeEventKind::WorkspacePatch(_)
                ));
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
            )
            .await
            .unwrap();
            assert_eq!(reductions.len(), 1);
            let published = receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(patch) = published.kind else {
                panic!("prefetched history must publish one typed patch");
            };
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

            let cached = SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("cached".into()),
                ..Default::default()
            };
            store
                .store_history("C1", std::slice::from_ref(&cached))
                .await
                .unwrap();
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
            let RuntimeEventKind::HistoryLoadCompleted {
                channel_id,
                has_more,
                next_cursor,
                append_older,
                cached: completion_cached,
            } = completion.kind
            else {
                panic!("fresh completion must follow both FIFO patches");
            };
            assert_eq!(channel_id, "C1");
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
            let stored_conversation = store
                .load_conversations()
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
                    StoreChange::ConversationAttentionObserved { .. },
                ]
            ));
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoadCompleted {
                has_more,
                next_cursor,
                append_older,
                cached,
                ..
            } = completion.kind
            else {
                panic!("older history must complete after its durable patch");
            };
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
            store
                .store_history("C1", std::slice::from_ref(&message))
                .await
                .unwrap();
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
                RuntimeEventKind::HistoryLoadCompleted { cached: true, .. }
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
                    RuntimeEventKind::HistoryLoadCompleted {
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
            let stored_conversation = store
                .load_conversations()
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
            let RuntimeEventKind::WorkspacePatch(bootstrap) = receiver.recv().await.unwrap().kind
            else {
                panic!("workspace bootstrap patch was not published");
            };
            let ui_session = crate::workspace_state::WorkspaceSessionState::default();
            ui_session.apply_workspace_patch(&bootstrap).unwrap();
            ui_session.view.borrow_mut().select_conversation("C1");

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
            let RuntimeEventKind::HistoryLoadCompleted { cached: true, .. } =
                cached_completion.kind
            else {
                panic!("cache hydration must complete before the network base is captured");
            };
            let network_base = workspace.revision();

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
                let RuntimeEventKind::WorkspacePatch(patch) = receiver.recv().await.unwrap().kind
                else {
                    panic!("realtime message patch was not published");
                };
                ui_session.apply_workspace_patch(&patch).unwrap();
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
            let RuntimeEventKind::WorkspacePatch(patch) = receiver.recv().await.unwrap().kind
            else {
                panic!("fresh history patch was not published");
            };
            ui_session.apply_workspace_patch(&patch).unwrap();
            let completion = receiver.recv().await.unwrap();
            let RuntimeEventKind::HistoryLoadCompleted {
                append_older: false,
                cached: false,
                ..
            } = completion.kind
            else {
                panic!("fresh history must complete after its canonical patch");
            };
            let projected_messages = ui_session
                .view
                .borrow()
                .channel_messages("C1")
                .iter()
                .map(|message| (message.ts.clone(), message.body_text()))
                .collect::<Vec<_>>();
            assert_eq!(
                projected_messages,
                vec![
                    ("5.0".into(), "fresh page item".into()),
                    ("4.0".into(), "concurrent post".into()),
                    ("1.0".into(), "authoritative edit".into()),
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
            let recovered = workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ReadAdvanced {
                        channel_id: "C1".into(),
                        ts: "20.0".into(),
                        remaining_unread: 0,
                    },
                )
                .await
                .unwrap();
            view.conversations
                .borrow_mut()
                .advance_read_cursor("C1", "20.0", 0);
            let local_reads = HashMap::from([("C1".to_string(), "20.0".to_string())]);
            assert_eq!(
                recovered
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
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
                recovered
                    .iter()
                    .map(|reduction| reduction.effects().len())
                    .collect::<Vec<_>>(),
                vec![1, 1, 1, 0],
                "effects from every recovered reduction must remain ordered"
            );

            let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert_eq!(delivered.len(), 4);
            let patches = delivered
                .into_iter()
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
                recovered
                    .iter()
                    .map(|reduction| reduction.patch().revision().value())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                patches
                    .iter()
                    .filter_map(|patch| {
                        patch.changes().iter().find_map(|change| match change {
                            WorkspaceChange::ConversationAttentionObserved {
                                observations, ..
                            } => observations
                                .first()
                                .map(|observation| observation.message_ts.as_str()),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>(),
                vec!["11.0", "12.0", "21.0"]
            );
            for patch in &patches {
                view.apply_workspace_patch_with_local_reads(patch, &local_reads)
                    .unwrap();
            }

            let view_current = view.conversations.borrow().get("C1").cloned().unwrap();
            let stored_current = store
                .load_conversations()
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
                store
                    .load_conversations()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .find(|conversation| conversation.id == "C1")
                    .unwrap()
                    .local_read_ts(),
                Some("20.0")
            );
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
            store
                .store_conversations(std::slice::from_ref(&initial))
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

            let admission = workspace.store_batch_admission.lock().await;
            let workspace_store = Some(store.clone());
            let local_read = publish_local_read_marker(
                &events,
                &workspace_store,
                &workspace,
                "C1",
                "20.0",
                ConversationReadMode::All,
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
            let before_release = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(before_release[0].local_read_ts(), None);
            assert_eq!(before_release[0].unread_activity_count(), 4);

            drop(admission);
            local_read.await;
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
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
            let persisted = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(persisted[0].local_read_ts(), Some("20.0"));
            assert_eq!(persisted[0].unread_activity_count(), 0);

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
            let persisted = store.load_conversations().await.unwrap().unwrap();
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
                        RuntimeEventKind::ConversationOpenCompleted {
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
                        RuntimeEventKind::ConversationOpenCompleted {
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
                    RuntimeEventKind::ConversationOpenCompleted { channel_id } => {
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
            assert!(store.load_conversations().await.unwrap().is_none());
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

            let stored = store.load_conversations().await.unwrap().unwrap();
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
            let stored = store.load_conversations().await.unwrap().unwrap();
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
            store
                .store_conversations(std::slice::from_ref(&initial))
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
            assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());

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
            assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
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
            assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let generation = store.recovery_generation();
            let base_revision = workspace.revision();
            assert!(!store.conversation_cache_needs_repair());
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
            assert!(!store.conversation_cache_needs_repair());
            assert_eq!(workspace.revision(), base_revision);
            let completion = receiver.recv().await.unwrap();
            assert_eq!(completion.meta.request, Some(RequestId::new(11)));
            assert!(matches!(
                completion.kind,
                RuntimeEventKind::ConversationsSynchronized
            ));
            assert!(receiver.try_recv().is_err());
            assert_eq!(
                store.load_conversations().await.unwrap().unwrap(),
                vec![conversation]
            );
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
        assert_eq!(workspace.revision().value(), 3);

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
        assert_eq!(workspace.revision().value(), 4);
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
        let live_attention = reduction
            .effects()
            .iter()
            .map(|effect| match effect {
                WorkspaceEffect::MessageAttention(effect) => effect,
            })
            .next();

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
        assert!(output.contains("admission=Coalescible"));
        assert!(output.contains("replacement_key=Some(Media(OpaqueAdmissionTarget))"));
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
            blocks_json: Some("do not trace these blocks".to_string()),
            thread_ts: None,
        };

        let fields = RuntimeTraceFields::for_command(identity, &command);

        assert_eq!(fields.session, identity.session);
        assert_eq!(fields.request, identity.request);
        assert_eq!(fields.operation, RuntimeOperation::PostMessage);
        assert_eq!(fields.target, "message:C123:main");
        assert!(!format!("{fields:?}").contains("do not trace this message"));
        assert!(!format!("{fields:?}").contains("do not trace these blocks"));

        let update = RuntimeCommand::UpdateMessage {
            channel_id: "C123".to_string(),
            original: Box::new(SlackMessage {
                ts: "1.0".to_string(),
                ..SlackMessage::default()
            }),
            text: "do not trace this edit".to_string(),
            blocks_json: Some("do not trace edited blocks".to_string()),
        };
        let fields = RuntimeTraceFields::for_command(identity, &update);
        assert_eq!(fields.operation, RuntimeOperation::UpdateMessage);
        assert_eq!(fields.target, "exact-message:C123:1.0");
        assert!(!format!("{fields:?}").contains("do not trace this edit"));
        assert!(!format!("{fields:?}").contains("do not trace edited blocks"));
    }

    #[test]
    fn updated_content_is_atomically_merged_with_current_workspace_metadata() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-local-message-update-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        let freshest = SlackMessage {
            user: Some("U1".into()),
            text: Some("old".into()),
            ts: "1.0".into(),
            reply_count: Some(4),
            reactions: Some(vec![crate::models::SlackReaction {
                name: Some("thumbsup".into()),
                count: Some(2),
                users: Some(vec!["U1".into(), "U2".into()]),
            }]),
            ..SlackMessage::default()
        };
        let mut updated = SlackMessage {
            user: Some("U1".into()),
            text: Some("edited".into()),
            ts: "1.0".into(),
            edited: Some(crate::models::SlackMessageEdit {
                user: Some("U1".into()),
                ts: None,
            }),
            ..SlackMessage::default()
        };
        updated.refresh_canonical_content();
        let original = SlackMessage {
            user: Some("U1".into()),
            text: Some("old".into()),
            ts: "1.0".into(),
            ..SlackMessage::default()
        };
        let workspace = WorkspaceReducerAdapter::default();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let merged = runtime.block_on(async {
            let (sender, _receiver) = mpsc::unbounded_channel();
            let events = RuntimeEventSender::new(
                sender,
                RuntimeIdentity {
                    session: SessionId::default().next(),
                    request: RequestId::new(1),
                },
                OperationContext::new(RuntimeOperation::UpdateMessage, RuntimeTarget::Workspace),
            );
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: freshest.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                )
                .await
                .unwrap();
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageUpdated {
                        channel_id: "C1".into(),
                        original: Box::new(original),
                        updated,
                    },
                )
                .await
                .unwrap();
            workspace.message("C1", "1.0").unwrap()
        });

        assert_eq!(merged.body_text(), "edited");
        assert_eq!(merged.reactions, freshest.reactions);
        assert_eq!(merged.reply_count, Some(4));
        assert!(merged.edited.is_some());
        assert_eq!(workspace.history("C1"), vec![merged]);
        runtime.block_on(async {
            let history = store.load_history("C1").await.unwrap().unwrap();
            let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
            let catalog = store.load_thread_catalog().await.unwrap();
            for cached in [&history[0], &thread[0]] {
                assert_eq!(cached.body_text(), "edited");
                assert_eq!(cached.reactions, freshest.reactions);
                assert_eq!(cached.reply_count, Some(4));
            }
            assert_eq!(catalog[0].root.as_ref().unwrap().body_text(), "edited");
        });
        std::fs::remove_dir_all(directory).unwrap();
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
    fn switching_main_navigation_aborts_old_target_and_starts_new_target() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let session = SessionId::default().next();
            let state = Arc::new(Mutex::new(RuntimeState::new(session)));
            let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
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
            let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
            let (first_cancelled_tx, first_cancelled_rx) = tokio::sync::oneshot::channel();
            let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();

            let first_limits = limits.clone();
            spawn_request_task(
                &state,
                TrackedRequest::for_command(first_identity, &first_command),
                async move {
                    let _permit = first_limits.acquire(RuntimeTaskLane::Navigation).await;
                    let _cancelled = CancellationSignal(Some(first_cancelled_tx));
                    let _ = first_started_tx.send(());
                    future::pending::<()>().await;
                },
            );
            first_started_rx
                .await
                .expect("first navigation did not start");

            let second_limits = limits;
            spawn_request_task(
                &state,
                TrackedRequest::for_command(second_identity, &second_command),
                async move {
                    let _permit = second_limits.acquire(RuntimeTaskLane::Navigation).await;
                    let _ = second_started_tx.send(());
                },
            );

            tokio::time::timeout(Duration::from_millis(100), first_cancelled_rx)
                .await
                .expect("abandoned navigation was not aborted")
                .expect("navigation cancellation signal dropped");
            tokio::time::timeout(Duration::from_millis(100), second_started_rx)
                .await
                .expect("new navigation did not get capacity")
                .expect("new navigation start signal dropped");
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
                blocks_json: None,
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
        assert!(!RuntimeCommand::UploadFiles {
            channel_id: "C1".to_string(),
            thread_ts: None,
            attachments: vec![UploadAttachment {
                path: PathBuf::from("example.txt"),
                remove_after_upload: false,
            }],
            blocks_json: None,
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
                state
                    .active_requests
                    .get(&context)
                    .map(|request| request.task_id),
                Some(2)
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
    fn message_completion_follows_its_canonical_workspace_patch() {
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
            let workspace = WorkspaceReducerAdapter::default();
            workspace
                .apply_persisted_and_publish_with_completion(
                    None,
                    &event_sender,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: message.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                    RuntimeEventKind::MessagePostCompleted {
                        channel_id: "C1".into(),
                        message_ts: message.ts.clone(),
                        thread_ts: None,
                    },
                )
                .await
                .unwrap();

            let patch = events
                .recv()
                .await
                .expect("workspace patch should be queued");
            assert!(matches!(patch.kind, RuntimeEventKind::WorkspacePatch(_)));
            let event = events.recv().await.expect("completion should follow patch");
            let RuntimeEventKind::MessagePostCompleted {
                channel_id,
                message_ts,
                thread_ts: None,
            } = event.kind
            else {
                panic!("expected message post completion event");
            };
            assert_eq!(channel_id, "C1");
            assert_eq!(message_ts, message.ts);
            assert!(events.try_recv().is_err());
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
            let RuntimeEventKind::HistoryLoadCompleted {
                has_more: true,
                next_cursor: Some(cursor),
                ..
            } = completion.kind
            else {
                panic!("fresh history completion was not queued");
            };
            assert_eq!(cursor, "older");
            assert_eq!(
                workspace
                    .history("C1")
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["01.0", "09.0", "10.0"]
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
        let history_commands = production
            .split_once("RuntimeCommand::LoadHistory")
            .unwrap()
            .1
            .split_once("RuntimeCommand::LoadThread")
            .unwrap()
            .0;
        let (latest, _older) = history_commands
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
        let service_source = include_str!("services/conversation_history.rs");
        let service_production = service_source.split_once("#[cfg(test)]").unwrap().0;
        assert!(!service_production.contains("async fn store_history"));
        assert!(!service_production.contains(".store_history("));
        assert!(!service_production.contains("CacheWriteFailed"));
    }

    async fn assert_history_completion_serializes_canonical_producer() {
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
        let producer = tokio::spawn(async move {
            producer_attempted.send(()).unwrap();
            let message = SlackMessage {
                ts: "2.0".into(),
                user: Some("U_SELF".into()),
                text: Some("direct producer".into()),
                ..Default::default()
            };
            producer_workspace
                .apply_persisted_and_publish_with_completion(
                    None,
                    &producer_events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: message.clone(),
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                    RuntimeEventKind::MessagePostCompleted {
                        channel_id: "C1".into(),
                        message_ts: message.ts,
                        thread_ts: None,
                    },
                )
                .await
                .unwrap();
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
        producer.await.unwrap();
        assert!(
            !producer_overtook_completion,
            "direct producer entered after the canonical clone but before HistoryLoadCompleted"
        );

        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let history_position = delivered
            .iter()
            .position(|event| matches!(event, RuntimeEventKind::HistoryLoadCompleted { .. }))
            .expect("history completion was not queued");
        let producer_position = delivered
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEventKind::WorkspacePatch(patch)
                        if timeline_patch_summary(patch.changes())
                            .iter()
                            .any(|(message_ts, _)| message_ts == "2.0")
                )
            })
            .expect("direct producer patch was not queued");
        assert!(history_position < producer_position);
    }

    #[test]
    fn history_patch_and_completion_are_atomic_against_canonical_message_producers() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to build test runtime");
        runtime.block_on(async {
            assert_history_completion_serializes_canonical_producer().await;
        });
    }

    #[test]
    fn realtime_persistence_channel_backpressures_at_capacity_and_preserves_fifo() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let metrics = Arc::new(AttentionMetrics::default());
            let (sender, mut receiver) =
                realtime_persistence_channel_with_capacity(Arc::clone(&metrics), 2);
            let event = |ts: &str| RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: ts.into(),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            };
            let event_ts = |event: RealtimePersistenceEvent| match event {
                RealtimePersistenceEvent::Message { event } => event.message.ts,
                other => panic!("expected message event, got {other:?}"),
            };

            sender.send(event("1.0")).await.unwrap();
            sender.send(event("2.0")).await.unwrap();
            let third_sender = sender.clone();
            let (attempted_sender, attempted_receiver) = oneshot::channel();
            let mut third_send = tokio::spawn(async move {
                let _ = attempted_sender.send(());
                third_sender.send(event("3.0")).await.unwrap();
            });
            attempted_receiver.await.unwrap();

            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut third_send)
                    .await
                    .is_err(),
                "send unexpectedly completed while the queue was full"
            );
            let saturated = metrics.snapshot();
            assert_eq!(saturated.queue_enqueued, 2);
            assert_eq!(saturated.queue_depth, 2);
            assert_eq!(saturated.queue_peak_depth, 2);
            assert_eq!(event_ts(receiver.recv().await.unwrap()), "1.0");

            third_send.await.unwrap();
            drop(sender);
            assert_eq!(event_ts(receiver.recv().await.unwrap()), "2.0");
            assert_eq!(event_ts(receiver.recv().await.unwrap()), "3.0");
            assert!(receiver.recv().await.is_none());

            let drained = metrics.snapshot();
            assert_eq!(drained.queue_enqueued, 3);
            assert_eq!(drained.queue_dequeued, 3);
            assert_eq!(drained.queue_depth, 0);
            assert_eq!(drained.queue_peak_depth, 2);
            assert_eq!(drained.queue_rejected, 0);
        });
    }

    #[test]
    fn realtime_persistence_drain_waits_for_fifo_and_reconciles_depth() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let workspace = WorkspaceReducerAdapter::default();
            let metrics = workspace.attention_metrics_handle();
            let (sender, mut receiver) =
                realtime_persistence_channel_with_capacity(Arc::clone(&metrics), 3);
            let observed = Arc::new(Mutex::new(Vec::new()));
            let observed_by_worker = Arc::clone(&observed);
            let (release_worker, worker_gate) = oneshot::channel();
            let mut persistence_tasks = tokio::task::JoinSet::new();
            persistence_tasks.spawn(async move {
                let _ = worker_gate.await;
                while let Some(event) = receiver.recv().await {
                    let RealtimePersistenceEvent::Message { event } = event else {
                        panic!("expected message event");
                    };
                    observed_by_worker
                        .lock()
                        .expect("observed event lock poisoned")
                        .push(event.message.ts);
                }
            });
            let event = |ts: &str| RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: ts.into(),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            };

            sender.send(event("1.0")).await.unwrap();
            sender.send(event("2.0")).await.unwrap();
            sender.send(event("3.0")).await.unwrap();
            assert_eq!(metrics.snapshot().queue_depth, 3);

            release_worker.send(()).unwrap();
            drain_realtime_persistence(Some(sender), &mut persistence_tasks, &workspace).await;

            assert_eq!(
                *observed.lock().expect("observed event lock poisoned"),
                ["1.0", "2.0", "3.0"]
            );
            let drained = metrics.snapshot();
            assert_eq!(drained.queue_enqueued, 3);
            assert_eq!(drained.queue_dequeued, 3);
            assert_eq!(drained.queue_depth, 0);
            assert_eq!(drained.queue_peak_depth, 3);
            assert_eq!(drained.queue_rejected, 0);
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
                .await
                .unwrap();
            sender
                .send(RealtimePersistenceEvent::UserChanged {
                    user: status_user("Stale"),
                    status_revision: Some(old_revision),
                })
                .await
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
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
            let admission = workspace.store_batch_admission.lock().await;
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
                .await
                .unwrap();
            sender
                .send(socket_message(
                    "1.0",
                    "edited",
                    SocketModeMessageKind::Changed,
                ))
                .await
                .unwrap();
            sender
                .send(socket_message(
                    "1.0",
                    "deleted",
                    SocketModeMessageKind::Deleted,
                ))
                .await
                .unwrap();
            sender
                .send(socket_message(
                    "2.0",
                    "survives",
                    SocketModeMessageKind::Posted,
                ))
                .await
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
                .flat_map(|event| match event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => {
                        timeline_patch_summary(patch.changes())
                    }
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                delivered,
                [
                    ("1.0".into(), Some("original".into())),
                    ("1.0".into(), Some("edited".into())),
                    ("1.0".into(), None),
                    ("2.0".into(), Some("survives".into())),
                ],
                "canonical timeline patches must retain socket FIFO order"
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
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
                .await
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
                "the pending read patch must publish before the current message patch"
            );
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::WorkspacePatch(patch)
                    if timeline_patch_summary(patch.changes())
                        == [("10.0".into(), Some("already read".into()))]
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
            let persisted_conversation = store
                .load_conversations()
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
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
                .await
                .unwrap();
            let first = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("recovery-failed message did not reach canonical UI")
                .expect("runtime event channel closed");
            assert!(matches!(first.kind, RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("10.0".into(), Some("must not claim".into()))]));
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
                .await
                .unwrap();
            let second = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("second recovery-failed message did not reach canonical UI")
                .expect("runtime event channel closed");
            assert!(
                matches!(second.kind, RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("30.0".into(), Some("new while recovery is blocked".into()))])
            );
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
                2
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
                .await
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let recovered_read = runtime_receiver.recv().await.unwrap();
            let recovered_stale_message = runtime_receiver.recv().await.unwrap();
            let recovered_new_message = runtime_receiver.recv().await.unwrap();
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
            assert!(
                matches!(current_patch.kind, RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("40.0".into(), Some("recovery trigger".into()))])
            );
            assert!(runtime_receiver.try_recv().is_err());

            let persisted_conversation = store
                .load_conversations()
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
    fn message_authority_uses_canonical_batches_and_keeps_queue_fallback_explicit() {
        let source = include_str!("runtime.rs");
        let post_command = source
            .split_once("RuntimeCommand::PostMessage {")
            .unwrap()
            .1
            .split_once("RuntimeCommand::SetReaction {")
            .unwrap()
            .0;
        assert!(post_command.contains("apply_persisted_and_publish_with_completion("));
        assert!(!post_command.contains("persist_local_post_message("));
        assert!(!post_command.contains("store_merged_history("));
        assert!(!post_command.contains("store_merged_thread("));

        let socket_persistence = source
            .split_once("async fn persist_socket_message(")
            .unwrap()
            .1
            .split_once("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
            .unwrap()
            .0;
        assert!(socket_persistence.contains("store_batch_admission.lock().await"));
        assert!(socket_persistence.contains("recover_persisted_admitted(Some(store))"));
        assert!(socket_persistence.contains("apply_and_enqueue("));
        assert!(!socket_persistence.contains("store_merged_history("));
        assert!(!socket_persistence.contains("store_merged_thread("));
        assert!(!socket_persistence.contains("workspace.apply("));
        assert!(!socket_persistence.contains("RuntimeEventKind::SocketModeEvent"));
        assert!(!socket_persistence.contains("observe_socket_thread_catalog("));

        let worker_message_path = source
            .split_once("RealtimePersistenceEvent::Message { event } => {")
            .unwrap()
            .1
            .split_once("RealtimePersistenceEvent::OrderedEvent { event } => {")
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
            .split_once("realtime_persistence_drain")
            .unwrap()
            .0;
        assert!(queue_fallback.contains("apply_realtime_persistence_queue_fallback("));
    }

    #[test]
    fn failed_socket_delta_recovers_typed_patches_fifo() {
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
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
            sender.send(self_post("1.0", "first")).await.unwrap();

            let visible = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
                .await
                .expect("unpersisted delta did not reach canonical UI")
                .expect("runtime event channel closed");
            let RuntimeEventKind::WorkspacePatch(visible_patch) = visible.kind else {
                panic!("unpersisted delta must publish its canonical patch");
            };
            assert_eq!(
                timeline_patch_summary(visible_patch.changes()),
                [("1.0".into(), Some("first".into()))]
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
            sender.send(self_post("2.0", "second")).await.unwrap();
            drop(sender);
            worker.await.unwrap();

            let recovered = runtime_receiver.recv().await.unwrap();
            let current = runtime_receiver.recv().await.unwrap();
            let RuntimeEventKind::WorkspacePatch(recovered_patch) = recovered.kind else {
                panic!("older failed delta patch must recover first");
            };
            let RuntimeEventKind::WorkspacePatch(current_patch) = current.kind else {
                panic!("current patch must follow the recovered patch");
            };
            assert_eq!(visible_patch.revision(), recovered_patch.revision());
            assert!(recovered_patch.revision() < current_patch.revision());
            assert_eq!(
                timeline_patch_summary(recovered_patch.changes()),
                [("1.0".into(), Some("first".into()))]
            );
            assert_eq!(
                timeline_patch_summary(current_patch.changes()),
                [("2.0".into(), Some("second".into()))]
            );
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
    fn socket_thread_catalog_patch_and_notification_order_is_explicit() {
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
            store
                .store_conversations(std::slice::from_ref(&conversation))
                .await
                .unwrap();
            store
                .store_history("D1", std::slice::from_ref(&root))
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
                .await
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .map(|event| event.kind)
                .collect::<Vec<_>>();
            assert!(matches!(
                delivered.as_slice(),
                [
                    RuntimeEventKind::WorkspacePatch(patch),
                    RuntimeEventKind::AttentionNotificationCandidate { message: notified, .. },
                ] if patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ThreadCatalogChanged(records) if !records.is_empty()
                    ))
                    && notified.ts == "2.0"
                    && timeline_patch_summary(patch.changes())
                        .iter()
                        .any(|(message_ts, body)| {
                            message_ts == "2.0" && body.as_deref() == Some("reply")
                        })
                    && patch.changes().iter().any(|change| matches!(
                        change,
                        WorkspaceChange::ConversationAttentionObserved { .. }
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
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "1.0".into(),
                            user: Some("U_SELF".into()),
                            text: Some("local channel post".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                )
                .await
                .unwrap();
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Local,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "2.0".into(),
                            thread_ts: Some("1.0".into()),
                            user: Some("U_SELF".into()),
                            text: Some("local reply".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                        origin: MutationOrigin::Local,
                    },
                )
                .await
                .unwrap();
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
            apply_realtime_persistence_queue_fallback(
                &workspace,
                &events,
                SocketModeEvent::Message(Box::new(fallback)),
            );
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
                RuntimeEventKind::WorkspacePatch(patch)
                    if timeline_patch_summary(patch.changes())
                        == [("3.0".into(), Some("queue fallback".into()))]
                        && patch.changes().iter().any(|change| matches!(
                            change,
                            WorkspaceChange::ConversationAttentionObserved { observations, .. }
                                if observations.iter().any(|observation| observation.record_unread)
                        ))
            )));
            assert!(delivered.iter().any(|event| matches!(
                event,
                RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                    if message.ts == "3.0"
            )));
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
                .await
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
        let delivered_timeline_changes = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .flat_map(|event| match event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => timeline_patch_summary(patch.changes()),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delivered_timeline_changes,
            [
                ("1.0".into(), Some("authoritative edit".into())),
                ("2.0".into(), None),
                ("3.0".into(), Some("authoritative thread location".into())),
                ("3.0".into(), None),
                ("4.0".into(), Some("concurrent post".into())),
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
            store
                .store_conversations(&[crate::models::SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }])
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
                    .await
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
                .await
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(history.len(), 2);
            let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
            assert_eq!(thread.len(), 1);
            assert_eq!(thread[0].ts, "3.0");
            let conversation = store.load_conversations().await.unwrap().unwrap().remove(0);
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
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }])
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
                .await
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
                .await
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
                .await
                .unwrap();
            drop(sender);
            worker.await.unwrap();

            let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
                .flat_map(|event| match event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => {
                        timeline_patch_summary(patch.changes())
                    }
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                delivered,
                [
                    ("1.0".into(), Some("message".into())),
                    ("1.0".into(), Some("message".into())),
                    ("1.0".into(), None),
                ]
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
            store
                .store_conversations(&[SlackConversation {
                    id: "D1".into(),
                    is_im: Some(true),
                    ..Default::default()
                }])
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
                    .await
                    .unwrap();
            }
            drop(sender);
            worker.await.unwrap();

            let mut notifications = 0;
            while let Ok(event) = receiver.try_recv() {
                if matches!(
                    event.kind,
                    RuntimeEventKind::AttentionNotificationCandidate { .. }
                ) {
                    notifications += 1;
                }
            }
            assert_eq!(notifications, 1);
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
            let conversation = store
                .load_conversations()
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
    fn realtime_attention_persistence_failure_queues_delta_and_publishes_typed_patch() {
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
                .await
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
            assert!(matches!(event.kind, RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("1.0".into(), Some("deferred".into()))]));
            assert!(
                receiver.try_recv().is_err(),
                "failed persistence must not emit duplicate projections"
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
            store.store_conversations(&conversations).await.unwrap();

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
                reply_count: Some(1),
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
            let mut persistence_events = Vec::with_capacity(1_200);
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
                    persistence_events.push(event);
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
            persistence_events.extend(duplicate_direct_messages);
            for event in persistence_events {
                sender
                    .send(RealtimePersistenceEvent::Message {
                        event: Box::new(event),
                    })
                    .await
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
            assert!((1..=REALTIME_PERSISTENCE_QUEUE_CAPACITY as u64).contains(&queue_peak));
            assert_eq!(metrics.queue_rejected, 0);

            let mut notification_events = 0;
            let mut message_patches = 0;
            while let Ok(event) = receiver.try_recv() {
                match event.kind {
                    RuntimeEventKind::AttentionNotificationCandidate { .. } => {
                        notification_events += 1;
                    }
                    RuntimeEventKind::WorkspacePatch(patch)
                        if !timeline_patch_summary(patch.changes()).is_empty() =>
                    {
                        message_patches += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(notification_events, 500);
            assert_eq!(message_patches, 1_000);

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
            let persisted = reopened_store.load_conversations().await.unwrap().unwrap();
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
    fn durable_read_rejection_keeps_timeline_without_restoring_attention() {
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
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }])
                .await
                .unwrap();
            assert!(store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap());

            let workspace = WorkspaceReducerAdapter::default();
            workspace.update_attention_context(WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".into()),
            });
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
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
                .await
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
            assert!(matches!(event.kind, RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("10.0".into(), Some("already read".into()))]));
            let coordinator = workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned");
            let conversation = coordinator.conversation("C1").unwrap();
            assert_eq!(conversation.unread_activity_count(), 0);
            assert!(conversation.has_observed_attention_message("10.0"));
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

            replace_session_and_drain(&state, second_session).await;

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
    fn realtime_wait_prioritizes_shutdown_over_ready_work() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let (shutdown, mut shutdown_receiver) = oneshot::channel();
            shutdown.send(()).unwrap();

            assert_eq!(
                wait_for_realtime_or_shutdown(&mut shutdown_receiver, future::ready(42)).await,
                None
            );
        });
    }

    #[test]
    fn session_replacement_marks_new_session_before_realtime_drain_completes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
            let (started, mut started_receiver) = oneshot::channel();
            let (drain_started, drain_started_receiver) = oneshot::channel();
            let (release_drain, release_drain_receiver) = oneshot::channel();
            let (drained, drained_receiver) = oneshot::channel();
            let supervisor =
                RealtimeSessionSupervisor::spawn(first_session, move |shutdown| async move {
                    let _ = started.send(());
                    let _ = shutdown.await;
                    let _ = drain_started.send(());
                    let _ = release_drain_receiver.await;
                    let _ = drained.send(());
                });

            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut started_receiver)
                    .await
                    .is_err(),
                "realtime supervisor started before installation"
            );
            assert!(state
                .lock()
                .expect("runtime state lock poisoned")
                .install_realtime_supervisor(supervisor)
                .is_ok());
            started_receiver
                .await
                .expect("installed realtime supervisor did not start");

            let state_for_replacement = Arc::clone(&state);
            let replacement = tokio::spawn(async move {
                replace_session_and_drain(&state_for_replacement, second_session).await;
            });
            drain_started_receiver
                .await
                .expect("realtime drain did not start");

            assert_eq!(
                state
                    .lock()
                    .expect("runtime state lock poisoned")
                    .active_session,
                second_session
            );
            assert!(
                !replacement.is_finished(),
                "session replacement completed before realtime drain"
            );

            let (old_task_started, old_task_started_receiver) = oneshot::channel();
            spawn_session_task(&state, first_session, async move {
                let _ = old_task_started.send(());
            });
            assert!(matches!(
                tokio::time::timeout(Duration::from_millis(100), old_task_started_receiver).await,
                Ok(Err(_))
            ));

            release_drain.send(()).unwrap();
            replacement.await.unwrap();
            drained_receiver
                .await
                .expect("realtime drain did not finish");
            assert!(state
                .lock()
                .expect("runtime state lock poisoned")
                .realtime
                .is_none());
        });
    }

    #[test]
    fn stale_realtime_supervisor_is_rejected_before_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            let first_session = SessionId::default().next();
            let second_session = first_session.next();
            let state = Arc::new(Mutex::new(RuntimeState::new(second_session)));
            let (started, started_receiver) = oneshot::channel();
            let supervisor =
                RealtimeSessionSupervisor::spawn(first_session, move |_shutdown| async move {
                    let _ = started.send(());
                });

            let rejected = {
                let mut state = state.lock().expect("runtime state lock poisoned");
                match state.install_realtime_supervisor(supervisor) {
                    Ok(()) => panic!("stale realtime supervisor was installed"),
                    Err(rejected) => rejected,
                }
            };
            rejected.shutdown().await;

            assert!(started_receiver.await.is_err());
            assert!(state
                .lock()
                .expect("runtime state lock poisoned")
                .realtime
                .is_none());
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

        assert!(state.begin_session_replacement(second_session).is_none());
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
            image_cache_scope: "test-workspace".to_string(),
            workspace: workspace.clone(),
            current_user_id: Some("U_SELF".into()),
            user_cache: Arc::new(Mutex::new(HashMap::new())),
            read_marks: Arc::new(Mutex::new(HashMap::new())),
            message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(256))),
            conversation_star_sync: ConversationStarSyncGate::default(),
            user_status_sync: UserStatusSync::default(),
            team_id: None,
            huddles,
            scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                SchedulerConfig::new(256, 8, 5).unwrap(),
            ))),
            pending_jobs: Arc::new(Mutex::new(HashMap::new())),
            next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
    fn completed_upload_cleanup_removes_only_owned_staged_files() {
        let owned = std::env::temp_dir().join(format!(
            "conduit-upload-cleanup-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let source = owned.with_extension("source");
        std::fs::write(&owned, b"screenshot").unwrap();
        std::fs::write(&source, b"source").unwrap();

        remove_completed_upload_files(&[
            UploadAttachment {
                path: owned.clone(),
                remove_after_upload: true,
            },
            UploadAttachment {
                path: source.clone(),
                remove_after_upload: false,
            },
        ]);

        assert!(!owned.exists());
        assert!(source.exists());
        std::fs::remove_file(source).unwrap();
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
        assert_eq!(preview_workspace_scope(&auth), "T123:U123");
    }

    #[test]
    fn preview_workspace_scope_uses_stable_trimmed_fallbacks() {
        let auth = AuthInfo {
            team: Some(" Ignored team ".to_string()),
            team_id: Some("   ".to_string()),
            url: Some(" https://example.slack.com ".to_string()),
            user: Some(" Ada ".to_string()),
            user_id: Some("  ".to_string()),
        };

        assert_eq!(
            preview_workspace_scope(&auth),
            "https://example.slack.com:Ada"
        );
        assert_eq!(
            preview_workspace_scope(&AuthInfo::default()),
            "unknown-team:unknown-user"
        );
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

    fn expected_runtime_admission(command: &RuntimeCommand) -> RuntimeAdmissionPolicy {
        match command {
            RuntimeCommand::LoadStoredToken
            | RuntimeCommand::StartOAuth { .. }
            | RuntimeCommand::StartBrowserSession { .. } => {
                RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Authentication)
            }
            RuntimeCommand::SignOut | RuntimeCommand::Disconnect => {
                RuntimeAdmissionPolicy::control()
            }
            RuntimeCommand::RefreshConversations => {
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::WorkspaceRefresh)
            }
            RuntimeCommand::DiscoverConversations => RuntimeAdmissionPolicy::coalescible(
                RuntimeAdmissionKey::ConversationDiscovery(ConversationDiscoveryScope::Full),
            ),
            RuntimeCommand::DiscoverChannels => RuntimeAdmissionPolicy::coalescible(
                RuntimeAdmissionKey::ConversationDiscovery(ConversationDiscoveryScope::Channels),
            ),
            RuntimeCommand::LoadHistory { .. }
            | RuntimeCommand::LoadOlderHistory { .. }
            | RuntimeCommand::SearchMessages { .. }
            | RuntimeCommand::LoadFiles
            | RuntimeCommand::LoadFile { .. }
            | RuntimeCommand::LoadSavedItems => RuntimeAdmissionPolicy::supersedable(
                RuntimeAdmissionKey::Navigation(NavigationSlot::Main),
            ),
            RuntimeCommand::LoadThread { .. } | RuntimeCommand::LoadOlderThread { .. } => {
                RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Navigation(
                    NavigationSlot::Thread,
                ))
            }
            RuntimeCommand::LoadMessageContext(location) => RuntimeAdmissionPolicy::supersedable(
                RuntimeAdmissionKey::Navigation(if location.thread_ts().is_some() {
                    NavigationSlot::Thread
                } else {
                    NavigationSlot::Main
                }),
            ),
            RuntimeCommand::LoadUser { user_id } => {
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                    scope: UserLoadScope::Basic,
                    target: OpaqueAdmissionTarget::digest(&[user_id]),
                })
            }
            RuntimeCommand::LoadUserProfile { user_id } => {
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                    scope: UserLoadScope::Profile,
                    target: OpaqueAdmissionTarget::digest(&[user_id]),
                })
            }
            RuntimeCommand::LoadImageAsset { key, .. } => RuntimeAdmissionPolicy::coalescible(
                RuntimeAdmissionKey::ImageAsset(OpaqueAdmissionTarget::digest(&[key])),
            ),
            RuntimeCommand::LoadMedia { url, .. } => RuntimeAdmissionPolicy::coalescible(
                RuntimeAdmissionKey::Media(OpaqueAdmissionTarget::digest(&[url])),
            ),
            RuntimeCommand::ResolveMessagePermalink { channel_id, ts } => {
                RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::MessagePermalink(
                    OpaqueAdmissionTarget::digest(&[channel_id, ts]),
                ))
            }
            RuntimeCommand::MarkConversationRead { .. }
            | RuntimeCommand::MarkConversationReadAll { .. }
            | RuntimeCommand::MarkThreadRead { .. } => RuntimeAdmissionPolicy::read_marker(),
            RuntimeCommand::UpdateAttentionPreferences(_)
            | RuntimeCommand::JoinConversation { .. }
            | RuntimeCommand::LeaveConversation { .. }
            | RuntimeCommand::OpenDirectMessage { .. }
            | RuntimeCommand::OpenGroupDirectMessage { .. }
            | RuntimeCommand::CreateChannel { .. }
            | RuntimeCommand::InviteToChannel { .. }
            | RuntimeCommand::DownloadAttachment { .. }
            | RuntimeCommand::ExecuteMessageAction { .. }
            | RuntimeCommand::PostMessage { .. }
            | RuntimeCommand::UpdateMessage { .. }
            | RuntimeCommand::SetReaction { .. }
            | RuntimeCommand::SetSaved { .. }
            | RuntimeCommand::SetConversationStarred { .. }
            | RuntimeCommand::SetCurrentUserStatus { .. }
            | RuntimeCommand::UploadFiles { .. }
            | RuntimeCommand::Huddle(_) => RuntimeAdmissionPolicy::durable_action(),
        }
    }

    fn expected_legacy_scheduling(
        command: &RuntimeCommand,
    ) -> (bool, Option<NavigationSlot>, RuntimeTaskLane) {
        match command {
            RuntimeCommand::LoadHistory { .. }
            | RuntimeCommand::LoadOlderHistory { .. }
            | RuntimeCommand::SearchMessages { .. }
            | RuntimeCommand::LoadFiles
            | RuntimeCommand::LoadFile { .. }
            | RuntimeCommand::LoadSavedItems => (
                true,
                Some(NavigationSlot::Main),
                RuntimeTaskLane::Navigation,
            ),
            RuntimeCommand::LoadThread { .. } | RuntimeCommand::LoadOlderThread { .. } => (
                true,
                Some(NavigationSlot::Thread),
                RuntimeTaskLane::Navigation,
            ),
            RuntimeCommand::LoadMessageContext(location) => (
                true,
                Some(if location.thread_ts().is_some() {
                    NavigationSlot::Thread
                } else {
                    NavigationSlot::Main
                }),
                RuntimeTaskLane::Navigation,
            ),
            RuntimeCommand::LoadStoredToken
            | RuntimeCommand::StartOAuth { .. }
            | RuntimeCommand::StartBrowserSession { .. }
            | RuntimeCommand::JoinConversation { .. }
            | RuntimeCommand::OpenDirectMessage { .. }
            | RuntimeCommand::ResolveMessagePermalink { .. }
            | RuntimeCommand::MarkConversationRead { .. } => {
                (true, None, RuntimeTaskLane::Interactive)
            }
            RuntimeCommand::RefreshConversations
            | RuntimeCommand::DiscoverChannels
            | RuntimeCommand::DiscoverConversations
            | RuntimeCommand::LoadUser { .. }
            | RuntimeCommand::LoadUserProfile { .. } => (true, None, RuntimeTaskLane::Background),
            RuntimeCommand::LoadImageAsset { .. }
            | RuntimeCommand::LoadMedia { .. }
            | RuntimeCommand::DownloadAttachment { .. } => (true, None, RuntimeTaskLane::Image),
            RuntimeCommand::UploadFiles { .. } => (false, None, RuntimeTaskLane::Upload),
            RuntimeCommand::SignOut
            | RuntimeCommand::Disconnect
            | RuntimeCommand::UpdateAttentionPreferences(_)
            | RuntimeCommand::LeaveConversation { .. }
            | RuntimeCommand::OpenGroupDirectMessage { .. }
            | RuntimeCommand::CreateChannel { .. }
            | RuntimeCommand::InviteToChannel { .. }
            | RuntimeCommand::ExecuteMessageAction { .. }
            | RuntimeCommand::MarkConversationReadAll { .. }
            | RuntimeCommand::MarkThreadRead { .. }
            | RuntimeCommand::PostMessage { .. }
            | RuntimeCommand::UpdateMessage { .. }
            | RuntimeCommand::SetReaction { .. }
            | RuntimeCommand::SetSaved { .. }
            | RuntimeCommand::SetConversationStarred { .. }
            | RuntimeCommand::SetCurrentUserStatus { .. }
            | RuntimeCommand::Huddle(_) => (false, None, RuntimeTaskLane::Interactive),
        }
    }

    fn runtime_command_fixtures() -> Vec<RuntimeCommand> {
        vec![
            RuntimeCommand::LoadStoredToken,
            RuntimeCommand::StartOAuth {
                client_id: "client".to_string(),
                debug_auth: false,
            },
            RuntimeCommand::StartBrowserSession {
                xoxc_token: "browser-token-canary".to_string(),
                xoxd_token: "cookie-token-canary".to_string(),
                user_agent: Some("agent-canary".to_string()),
            },
            RuntimeCommand::SignOut,
            RuntimeCommand::Disconnect,
            RuntimeCommand::RefreshConversations,
            RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
            RuntimeCommand::DiscoverChannels,
            RuntimeCommand::DiscoverConversations,
            RuntimeCommand::JoinConversation {
                channel_id: "C1".to_string(),
            },
            RuntimeCommand::LeaveConversation {
                channel_id: "C1".to_string(),
            },
            RuntimeCommand::OpenDirectMessage {
                user_id: "U1".to_string(),
            },
            RuntimeCommand::OpenGroupDirectMessage {
                user_ids: vec!["U1".to_string(), "U2".to_string()],
            },
            RuntimeCommand::CreateChannel {
                name: "channel".to_string(),
                is_private: false,
            },
            RuntimeCommand::InviteToChannel {
                channel_id: "C1".to_string(),
                user_ids: vec!["U1".to_string()],
            },
            RuntimeCommand::LoadHistory {
                channel_id: "C1".to_string(),
            },
            RuntimeCommand::LoadOlderHistory {
                channel_id: "C1".to_string(),
                cursor: "cursor".to_string(),
            },
            RuntimeCommand::LoadThread {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            },
            RuntimeCommand::LoadOlderThread {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
                cursor: "cursor".to_string(),
            },
            RuntimeCommand::LoadMessageContext(
                SearchMessageLocation::new("C1", "1.0", None).unwrap(),
            ),
            RuntimeCommand::SearchMessages {
                query: "query-canary".to_string(),
            },
            RuntimeCommand::LoadFiles,
            RuntimeCommand::LoadFile {
                file_id: "F1".to_string(),
                share_requested: false,
            },
            RuntimeCommand::LoadSavedItems,
            RuntimeCommand::LoadUser {
                user_id: "U1".to_string(),
            },
            RuntimeCommand::LoadUserProfile {
                user_id: "U1".to_string(),
            },
            RuntimeCommand::LoadImageAsset {
                key: "image-key-canary".to_string(),
                url: "https://files.slack.com/image?token=image-url-canary".to_string(),
            },
            RuntimeCommand::LoadMedia {
                url: "https://files.slack.com/media?token=media-url-canary".to_string(),
                name: "media-name-canary".to_string(),
            },
            RuntimeCommand::DownloadAttachment {
                url: "https://files.slack.com/download?token=download-url-canary".to_string(),
                name: "download-name-canary".to_string(),
            },
            RuntimeCommand::ResolveMessagePermalink {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            },
            RuntimeCommand::ExecuteMessageAction {
                request: SlackMessageActionRequest {
                    channel_id: "C1".to_string(),
                    message_ts: "1.0".to_string(),
                    thread_ts: None,
                    service_id: "B1".to_string(),
                    app_id: None,
                    bot_user_id: None,
                    action: crate::rich_message::SlackControlAction::Block {
                        action: crate::rich_message::SensitiveValue::new("action-canary"),
                    },
                },
                control_handle: MessageControlHandle::synthetic(),
            },
            RuntimeCommand::MarkConversationRead {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            },
            RuntimeCommand::MarkConversationReadAll {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            },
            RuntimeCommand::MarkThreadRead {
                channel_id: "C1".to_string(),
                thread_ts: "1.0".to_string(),
                ts: "2.0".to_string(),
            },
            RuntimeCommand::PostMessage {
                channel_id: "C1".to_string(),
                text: "message-text-canary".to_string(),
                blocks_json: Some("blocks-canary".to_string()),
                thread_ts: None,
            },
            RuntimeCommand::UpdateMessage {
                channel_id: "C1".to_string(),
                original: Box::new(SlackMessage {
                    ts: "1.0".to_string(),
                    ..SlackMessage::default()
                }),
                text: "edit-text-canary".to_string(),
                blocks_json: Some("edit-blocks-canary".to_string()),
            },
            RuntimeCommand::SetReaction {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
                name: "reaction-canary".to_string(),
                add: true,
                thread_ts: None,
            },
            RuntimeCommand::SetSaved {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
                add: true,
                thread_ts: None,
            },
            RuntimeCommand::SetConversationStarred {
                channel_id: "C1".to_string(),
                starred: true,
            },
            RuntimeCommand::SetCurrentUserStatus {
                status: SlackUserStatus::default(),
            },
            RuntimeCommand::UploadFiles {
                channel_id: "C1".to_string(),
                thread_ts: None,
                attachments: vec![UploadAttachment {
                    path: PathBuf::from("upload-path-canary"),
                    remove_after_upload: false,
                }],
                blocks_json: Some("upload-blocks-canary".to_string()),
            },
            RuntimeCommand::Huddle(HuddleCommand::SetMuted(true)),
        ]
    }

    #[test]
    fn runtime_command_admission_metadata_is_exhaustive_and_behavior_neutral() {
        let commands = runtime_command_fixtures();
        assert_eq!(commands.len(), 42);

        for command in commands {
            let descriptor = command.descriptor();
            assert_eq!(descriptor.admission, expected_runtime_admission(&command));

            let (supersedes_previous, navigation_slot, lane) = expected_legacy_scheduling(&command);
            assert_eq!(descriptor.supersedes_previous, supersedes_previous);
            assert_eq!(descriptor.navigation_slot, navigation_slot);
            assert_eq!(descriptor.lane, lane);

            assert_eq!(
                descriptor.admission.replacement_key.is_some(),
                matches!(
                    descriptor.admission.kind,
                    RuntimeAdmissionKind::Coalescible | RuntimeAdmissionKind::Supersedable
                )
            );
        }
    }

    #[test]
    fn every_huddle_command_is_a_durable_action() {
        let commands = [
            HuddleCommand::OpenPreflight {
                call_id: "R1".to_string(),
            },
            HuddleCommand::Join {
                call_id: "R1".to_string(),
            },
            HuddleCommand::OpenExternally {
                call_id: "R1".to_string(),
            },
            HuddleCommand::Leave,
            HuddleCommand::Dismiss,
            HuddleCommand::SetMuted(true),
            HuddleCommand::SetCameraEnabled(true),
            HuddleCommand::SetScreenShareEnabled(true),
            HuddleCommand::SelectDevice {
                kind: crate::huddles::state::HuddleDeviceKind::Microphone,
                id: "device-canary".to_string(),
            },
        ];

        for command in commands {
            assert_eq!(
                huddle_admission_policy(&command),
                RuntimeAdmissionPolicy::durable_action()
            );
        }
    }

    #[test]
    fn runtime_admission_keys_partition_replaceable_work() {
        let key = |command: RuntimeCommand| {
            command
                .descriptor()
                .admission
                .replacement_key
                .expect("replaceable command should have a key")
        };

        let main_navigation = key(RuntimeCommand::LoadHistory {
            channel_id: "C1".to_string(),
        });
        assert_eq!(
            main_navigation,
            key(RuntimeCommand::SearchMessages {
                query: "different query".to_string(),
            })
        );
        let thread_navigation = key(RuntimeCommand::LoadThread {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        });
        assert_eq!(
            thread_navigation,
            key(RuntimeCommand::LoadMessageContext(
                SearchMessageLocation::new("C1", "2.0", Some("1.0")).unwrap(),
            ))
        );
        assert_ne!(main_navigation, thread_navigation);

        assert_eq!(
            key(RuntimeCommand::LoadStoredToken),
            key(RuntimeCommand::StartBrowserSession {
                xoxc_token: "token-a".to_string(),
                xoxd_token: "token-b".to_string(),
                user_agent: None,
            })
        );
        assert_ne!(
            key(RuntimeCommand::DiscoverConversations),
            key(RuntimeCommand::DiscoverChannels)
        );
        assert_ne!(
            key(RuntimeCommand::LoadUser {
                user_id: "U1".to_string(),
            }),
            key(RuntimeCommand::LoadUserProfile {
                user_id: "U1".to_string(),
            })
        );
        assert_ne!(
            key(RuntimeCommand::LoadUser {
                user_id: "U1".to_string(),
            }),
            key(RuntimeCommand::LoadUser {
                user_id: "U2".to_string(),
            })
        );
        assert_ne!(
            key(RuntimeCommand::LoadImageAsset {
                key: "asset-a".to_string(),
                url: "https://files.slack.com/a".to_string(),
            }),
            key(RuntimeCommand::LoadImageAsset {
                key: "asset-b".to_string(),
                url: "https://files.slack.com/a".to_string(),
            })
        );
        // The viewer matches completion by URL, so one completion serves duplicate names.
        assert_eq!(
            key(RuntimeCommand::LoadMedia {
                url: "https://files.slack.com/media-a".to_string(),
                name: "first-name.mp4".to_string(),
            }),
            key(RuntimeCommand::LoadMedia {
                url: "https://files.slack.com/media-a".to_string(),
                name: "second-name.mp4".to_string(),
            })
        );
        assert_ne!(
            key(RuntimeCommand::LoadMedia {
                url: "https://files.slack.com/media-a".to_string(),
                name: "media.mp4".to_string(),
            }),
            key(RuntimeCommand::LoadMedia {
                url: "https://files.slack.com/media-b".to_string(),
                name: "media.mp4".to_string(),
            })
        );
        assert_ne!(
            key(RuntimeCommand::ResolveMessagePermalink {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            }),
            key(RuntimeCommand::ResolveMessagePermalink {
                channel_id: "C1".to_string(),
                ts: "2.0".to_string(),
            })
        );
    }

    #[test]
    fn runtime_admission_regressions_preserve_durable_intent() {
        let policy = |command: RuntimeCommand| command.descriptor().admission;

        assert_eq!(
            policy(RuntimeCommand::JoinConversation {
                channel_id: "C1".to_string(),
            })
            .kind,
            RuntimeAdmissionKind::DurableAction
        );
        assert_eq!(
            policy(RuntimeCommand::OpenDirectMessage {
                user_id: "U1".to_string(),
            })
            .kind,
            RuntimeAdmissionKind::DurableAction
        );
        assert_eq!(
            policy(RuntimeCommand::MarkConversationRead {
                channel_id: "C1".to_string(),
                ts: "1.0".to_string(),
            })
            .kind,
            RuntimeAdmissionKind::ReadMarker
        );
        assert_eq!(
            policy(RuntimeCommand::RefreshConversations).kind,
            RuntimeAdmissionKind::Coalescible
        );
    }

    #[test]
    fn runtime_admission_debug_output_excludes_command_payloads() {
        let debug = runtime_command_fixtures()
            .iter()
            .map(|command| format!("{:?}", command.descriptor()))
            .collect::<Vec<_>>()
            .join("\n");

        for private in [
            "browser-token-canary",
            "cookie-token-canary",
            "agent-canary",
            "query-canary",
            "image-key-canary",
            "image-url-canary",
            "media-url-canary",
            "media-name-canary",
            "download-url-canary",
            "download-name-canary",
            "action-canary",
            "message-text-canary",
            "blocks-canary",
            "edit-text-canary",
            "edit-blocks-canary",
            "reaction-canary",
            "upload-path-canary",
            "upload-blocks-canary",
        ] {
            assert!(!debug.contains(private), "descriptor leaked {private}");
        }
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

        let upload = RuntimeCommand::UploadFiles {
            channel_id: "C123".to_string(),
            thread_ts: None,
            attachments: vec![UploadAttachment {
                path: PathBuf::from("upload.png"),
                remove_after_upload: false,
            }],
            blocks_json: None,
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

        let explicit_mark_all = RuntimeCommand::MarkConversationReadAll {
            channel_id: "C123".to_string(),
            ts: "1710000000.000100".to_string(),
        }
        .descriptor();
        assert_eq!(explicit_mark_all.lane, RuntimeTaskLane::Interactive);
        assert!(!explicit_mark_all.supersedes_previous);

        let permalink = RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C123".to_string(),
            ts: "1710000000.000100".to_string(),
        }
        .descriptor();
        assert_eq!(permalink.lane, RuntimeTaskLane::Interactive);
        assert!(permalink.supersedes_previous);

        let message_action = RuntimeCommand::ExecuteMessageAction {
            request: SlackMessageActionRequest {
                channel_id: "C123".to_string(),
                message_ts: "1710000000.000100".to_string(),
                thread_ts: None,
                service_id: "B123".to_string(),
                app_id: Some("A123".to_string()),
                bot_user_id: Some("U123".to_string()),
                action: crate::rich_message::SlackControlAction::Block {
                    action: crate::rich_message::SensitiveValue::new(
                        r#"{"type":"button","block_id":"block","action_id":"approve"}"#,
                    ),
                },
            },
            control_handle: MessageControlHandle::synthetic(),
        }
        .descriptor();
        assert_eq!(message_action.lane, RuntimeTaskLane::Interactive);
        assert!(!message_action.supersedes_previous);
        assert_eq!(
            message_action.context,
            OperationContext::new(
                RuntimeOperation::MessageAction,
                RuntimeTarget::ExactMessage {
                    channel_id: "C123".to_string(),
                    ts: "1710000000.000100".to_string(),
                },
            )
        );

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
    fn follow_up_workspace_sync_jobs_have_distinct_cancellation_ids() {
        let startup = sync_job_cancellation_id(&SyncJobPayload::WorkspaceStartup, 0);
        let refresh = sync_job_cancellation_id(&SyncJobPayload::WorkspaceRefresh, 1);
        let directory = sync_job_cancellation_id(
            &SyncJobPayload::MembershipSync {
                channel_id: "user_directory".to_string(),
            },
            2,
        );

        assert_ne!(startup, refresh);
        assert_ne!(startup, directory);
        assert_ne!(refresh, directory);
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
            store.store_conversation(&initial).await.unwrap();
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
                "neither patch nor completion may precede persistence"
            );
            assert!(workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred());
            assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
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
            store.store_conversation(&initial).await.unwrap();
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

            store.store_conversation(&initial).await.unwrap();
            workspace.apply(
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(initial.clone()),
            );
            drop(refresh_guard);
            toggle.await.unwrap();

            let persisted = store
                .load_conversations()
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
                RuntimeEventKind::ConversationStarUpdateCompleted {
                    channel_id,
                    starred: true,
                } if channel_id == "C1"
            ));

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

        let event = RuntimeEventKind::HistoryLoadCompleted {
            channel_id: "C123".to_string(),
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

        let event = RuntimeEventKind::MessageContextLoadCompleted {
            location: SearchMessageLocation::new(
                "C123",
                "1710000001.000100",
                Some("1710000000.000100"),
            )
            .unwrap(),
            message_timestamps: Vec::new(),
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

        let event = RuntimeEventKind::CurrentUserStatusUpdateCompleted {
            user_id: "U123".to_string(),
            cleared: false,
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
            store
                .store_conversations(&[cached.clone(), removed])
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
            let mut persisted = store.load_conversations().await.unwrap().unwrap();
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
    fn preview_asset_cache_round_trips_raw_workspace_scoped_assets() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-image-cache-test-{}-{unique}",
            std::process::id()
        ));
        let cache = ImageAssetCache::new(directory.clone());
        let workspace = "T123:U123";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            assert_eq!(
                cache
                    .load(workspace, "https://files.example/image.png")
                    .await
                    .expect("cache load failed"),
                None
            );

            let image_bytes = b"\x89PNG\r\n\x1a\nraw-image".to_vec();
            let image = cache
                .store(
                    workspace,
                    "https://files.example/image.png",
                    DownloadedPreviewAsset {
                        mime_type: PreviewAssetMime::Png,
                        bytes: image_bytes.clone(),
                    },
                )
                .await
                .expect("cache store failed");

            let image_path = cache.path_for_key(
                workspace,
                "https://files.example/image.png",
                PreviewAssetMime::Png,
            );
            let cached = tokio::fs::read(&image_path).await.unwrap();
            assert!(!cached.starts_with(b"data:"));
            assert_eq!(cached, image_bytes);
            assert_eq!(image.path_in(&directory), image_path);
            assert_eq!(image.content_type(), "image/png");
            assert!(!image.is_video());

            assert_eq!(
                cache
                    .load(workspace, "https://files.example/image.png")
                    .await
                    .expect("cache load failed"),
                Some(image.clone())
            );
            assert_eq!(
                cache
                    .load("T999:U999", "https://files.example/image.png")
                    .await
                    .expect("cross-workspace cache load failed"),
                None
            );
            assert_ne!(
                image_path,
                cache.path_for_key(
                    "T999:U999",
                    "https://files.example/image.png",
                    PreviewAssetMime::Png,
                )
            );

            let gif = cache
                .store(
                    workspace,
                    "https://files.example/animated.gif",
                    DownloadedPreviewAsset {
                        mime_type: PreviewAssetMime::Gif,
                        bytes: b"GIF89a-animated-frames".to_vec(),
                    },
                )
                .await
                .expect("cache store failed");
            assert_eq!(
                cache
                    .load(workspace, "https://files.example/animated.gif")
                    .await
                    .expect("cache load failed"),
                Some(gif)
            );

            let video = cache
                .store(
                    workspace,
                    "https://files.example/video.mp4",
                    DownloadedPreviewAsset {
                        mime_type: PreviewAssetMime::Mp4,
                        bytes: b"\0\0\0\x18ftypisomvideo".to_vec(),
                    },
                )
                .await
                .expect("cache store failed");
            assert!(video.is_video());
            assert_eq!(
                cache
                    .load(workspace, "https://files.example/video.mp4")
                    .await
                    .expect("cache load failed"),
                Some(video)
            );

            let oversized_path = cache.path_for_key(workspace, "oversized", PreviewAssetMime::Png);
            let oversized_file = std::fs::File::create(&oversized_path).unwrap();
            oversized_file
                .set_len(PreviewAssetMime::Png.max_bytes() as u64 + 1)
                .unwrap();
            assert!(cache.load(workspace, "oversized").await.is_err());
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn preview_asset_store_rolls_back_when_bounds_cannot_be_enforced() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-image-cache-rollback-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let cache = ImageAssetCache::new(directory.clone());
        let workspace = "T123:U123";
        let key = "https://files.example/image.png";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            assert!(cache
                .store_with_policy(
                    workspace,
                    key,
                    DownloadedPreviewAsset {
                        mime_type: PreviewAssetMime::Png,
                        bytes: b"\x89PNG\r\n\x1a\nraw-image".to_vec(),
                    },
                    PreviewCachePolicy {
                        max_age: Duration::MAX,
                        max_bytes: u64::MAX,
                        max_entries: 0,
                    },
                )
                .await
                .is_err());
        });

        assert!(!cache
            .path_for_key(workspace, key, PreviewAssetMime::Png)
            .exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cached_asset_descriptor_rejects_invalid_keys_and_sizes() {
        let workspace_key = "a".repeat(64);
        let cache_key = "b".repeat(64);
        assert!(CachedAssetDescriptor::new(
            workspace_key.clone(),
            cache_key.clone(),
            PreviewAssetMime::Png,
            8,
        )
        .is_some());
        assert!(CachedAssetDescriptor::new(
            "../workspace".to_string(),
            cache_key.clone(),
            PreviewAssetMime::Png,
            8,
        )
        .is_none());
        assert!(CachedAssetDescriptor::new(
            workspace_key,
            "A".repeat(64),
            PreviewAssetMime::Png,
            8,
        )
        .is_none());
        assert!(CachedAssetDescriptor::new(
            "a".repeat(64),
            cache_key,
            PreviewAssetMime::Png,
            PreviewAssetMime::Png.max_bytes() as u64 + 1,
        )
        .is_none());
    }

    #[test]
    fn preview_cache_prunes_legacy_files_and_evicts_to_a_byte_cap() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-preview-prune-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let workspace = directory.join("a".repeat(64));
        std::fs::create_dir_all(&workspace).unwrap();
        let first = workspace.join(format!("{}.png", "1".repeat(64)));
        let second = workspace.join(format!("{}.png", "2".repeat(64)));
        let protected = workspace.join(format!("{}.png", "3".repeat(64)));
        let legacy = directory.join(format!("{}.data-uri", "4".repeat(64)));
        std::fs::write(&first, b"\x89PNG\r\n\x1a\n1").unwrap();
        std::fs::write(&second, b"\x89PNG\r\n\x1a\n2").unwrap();
        std::fs::write(&protected, b"\x89PNG\r\n\x1a\n3").unwrap();
        std::fs::write(&legacy, b"data:image/png;base64,legacy").unwrap();

        prune_preview_cache(
            &directory,
            Some(&protected),
            PreviewCachePolicy {
                max_age: Duration::MAX,
                max_bytes: 17,
                max_entries: 2,
            },
            SystemTime::now(),
        )
        .unwrap();

        let retained_size = std::fs::read_dir(&workspace)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>();
        assert!(!legacy.exists());
        assert!(protected.exists());
        assert!(retained_size <= 17);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preview_cache_evicts_to_an_entry_cap() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-preview-entry-prune-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let workspace = directory.join("a".repeat(64));
        std::fs::create_dir_all(&workspace).unwrap();
        for marker in ['1', '2', '3'] {
            std::fs::write(
                workspace.join(format!("{}.jpg", marker.to_string().repeat(64))),
                b"\xff\xd8\xff",
            )
            .unwrap();
        }

        prune_preview_cache(
            &directory,
            None,
            PreviewCachePolicy {
                max_age: Duration::MAX,
                max_bytes: u64::MAX,
                max_entries: 2,
            },
            SystemTime::now(),
        )
        .unwrap();

        assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preview_cache_reports_an_unenforceable_protected_bound() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-preview-protected-prune-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let workspace = directory.join("a".repeat(64));
        std::fs::create_dir_all(&workspace).unwrap();
        let protected = workspace.join(format!("{}.jpg", "1".repeat(64)));
        std::fs::write(&protected, b"\xff\xd8\xff").unwrap();

        assert!(prune_preview_cache(
            &directory,
            Some(&protected),
            PreviewCachePolicy {
                max_age: Duration::MAX,
                max_bytes: u64::MAX,
                max_entries: 0,
            },
            SystemTime::now(),
        )
        .is_err());
        assert!(protected.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn streaming_preview_eviction_is_independent_of_scan_order() {
        for (case, scan_order) in [[2, 1, 0], [0, 1, 2]].into_iter().enumerate() {
            let directory = std::env::temp_dir().join(format!(
                "conduit-preview-order-test-{}-{}-{case}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let sizes = [4_u64, 6, 6];
            let paths = sizes
                .iter()
                .enumerate()
                .map(|(index, size)| {
                    let path = directory.join(format!("asset-{index}.jpg"));
                    std::fs::write(&path, vec![0_u8; *size as usize]).unwrap();
                    path
                })
                .collect::<Vec<_>>();
            let mut retained = BTreeMap::new();
            let mut identities = HashMap::new();
            let mut total = 0;
            let mut eviction_cutoff = None;
            for index in scan_order {
                retain_preview_cache_entry(
                    &mut retained,
                    &mut identities,
                    &mut total,
                    &mut eviction_cutoff,
                    PreviewCacheEntry {
                        path: paths[index].clone(),
                        workspace_key: "workspace".to_string(),
                        cache_key: format!("key-{index}"),
                        size: sizes[index],
                        last_used: UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
                    },
                    None,
                    PreviewCachePolicy {
                        max_age: Duration::MAX,
                        max_bytes: 10,
                        max_entries: 10,
                    },
                )
                .unwrap();
            }

            assert_eq!(
                retained
                    .values()
                    .map(|entry| entry.cache_key.as_str())
                    .collect::<Vec<_>>(),
                vec!["key-2"]
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn streaming_preview_duplicates_keep_the_same_newest_winner() {
        for (case, scan_order) in [[0, 1, 2], [2, 0, 1], [1, 2, 0]].into_iter().enumerate() {
            let directory = std::env::temp_dir().join(format!(
                "conduit-preview-duplicate-test-{}-{}-{case}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let paths = ["png", "jpg", "gif"].map(|extension| {
                let path = directory.join(format!("asset.{extension}"));
                std::fs::write(&path, b"data").unwrap();
                path
            });
            let mut retained = BTreeMap::new();
            let mut identities = HashMap::new();
            let mut total = 0;
            let mut eviction_cutoff = None;
            for index in scan_order {
                retain_preview_cache_entry(
                    &mut retained,
                    &mut identities,
                    &mut total,
                    &mut eviction_cutoff,
                    PreviewCacheEntry {
                        path: paths[index].clone(),
                        workspace_key: "workspace".to_string(),
                        cache_key: "same-key".to_string(),
                        size: 4,
                        last_used: UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
                    },
                    None,
                    PreviewCachePolicy {
                        max_age: Duration::MAX,
                        max_bytes: 100,
                        max_entries: 10,
                    },
                )
                .unwrap();
            }

            assert_eq!(retained.len(), 1);
            assert_eq!(retained.values().next().unwrap().path, paths[2]);
            assert!(paths[2].exists());
            assert!(!paths[0].exists());
            assert!(!paths[1].exists());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
