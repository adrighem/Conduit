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
use tokio::sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore};
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
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation, SlackFile,
    SlackMessage, SlackUser, SlackUserGroup, SlackUserStatus, StoredToken,
};
use crate::realtime::RealtimeStatus;
use crate::services::conversation_history::ConversationHistoryService;
use crate::slack::{
    DownloadedPreviewAsset, PreviewAssetMime, SlackApi, SlackError, SlackErrorCategory,
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
    ConversationMembershipSnapshot, MessageAttentionEffect, MessageMutationKind, MutationOrigin,
    SnapshotEnvelope, StoreBatch, StoreChange, WorkspaceAttentionContext, WorkspaceBootstrapData,
    WorkspaceCoordinator, WorkspaceEffect, WorkspaceMutation, WorkspacePatch, WorkspaceReduction,
    WorkspaceRevision,
};
use crate::workspace_state::WorkspaceLifecycleEvent;

const CHANNEL_HISTORY_PREFETCH_LIMIT: usize = 12;
const CONVERSATION_MEMBER_PAGE_LIMIT: usize = 25;
const NAVIGATION_TASK_CONCURRENCY: usize = 2;
const INTERACTIVE_TASK_CONCURRENCY: usize = 8;
const BACKGROUND_TASK_CONCURRENCY: usize = 3;
const IMAGE_TASK_CONCURRENCY: usize = 4;
const UPLOAD_TASK_CONCURRENCY: usize = 2;
const RUNTIME_CONTROL_QUEUE_CAPACITY: usize = 8;
const RUNTIME_NAVIGATION_QUEUE_CAPACITY: usize = 16;
const RUNTIME_INTERACTIVE_QUEUE_CAPACITY: usize = 128;
const RUNTIME_BACKGROUND_QUEUE_CAPACITY: usize = 128;
const RUNTIME_IMAGE_QUEUE_CAPACITY: usize = 256;
const RUNTIME_UPLOAD_QUEUE_CAPACITY: usize = 32;
const RUNTIME_NAVIGATION_TASK_CAPACITY: usize = 4;
const RUNTIME_INTERACTIVE_TASK_CAPACITY: usize = 32;
const RUNTIME_BACKGROUND_TASK_CAPACITY: usize = 12;
const RUNTIME_IMAGE_TASK_CAPACITY: usize = 32;
const RUNTIME_UPLOAD_TASK_CAPACITY: usize = 8;
const RUNTIME_CONTROL_DISPATCH_BURST: usize = 4;
const HUDDLE_ACTOR_QUEUE_CAPACITY: usize = 64;
const HUDDLE_ACTOR_RESERVED_CAPACITY: usize = 8;
const HUDDLE_ACTOR_OBSERVATION_CAPACITY: usize =
    HUDDLE_ACTOR_QUEUE_CAPACITY - HUDDLE_ACTOR_RESERVED_CAPACITY;
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
pub use crate::runtime_mailbox::*;

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
    #[cfg(test)]
    fn for_command(identity: RuntimeIdentity, command: &RuntimeCommand) -> Self {
        Self::for_descriptor(identity, &command.descriptor())
    }

    fn for_descriptor(identity: RuntimeIdentity, descriptor: &RuntimeCommandDescriptor) -> Self {
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
            Self::LoadConversationDetails { channel_id } => RuntimeCommandDescriptor::request(
                channel(RuntimeOperation::ConversationDetails, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::SetConversationText { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::ConversationEdit, channel_id),
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
            Self::LoadFollowedThreads { .. } => RuntimeCommandDescriptor::navigation(
                workspace(RuntimeOperation::FollowedThreads),
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
            Self::ExecuteSlashCommand {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::ExecuteSlashCommand,
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
            Self::MarkConversationRead { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::MarkRead, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::MarkConversationUnread { channel_id, .. } => RuntimeCommandDescriptor::mutation(
                channel(RuntimeOperation::MarkUnread, channel_id),
                RuntimeTaskLane::Interactive,
                RuntimeAdmissionPolicy::durable_action(),
            ),
            Self::MarkThreadRead {
                channel_id,
                thread_ts,
                ..
            } => RuntimeCommandDescriptor::mutation(
                OperationContext::new(
                    RuntimeOperation::MarkThreadRead,
                    RuntimeTarget::Thread {
                        channel_id: channel_id.clone(),
                        thread_ts: thread_ts.clone(),
                    },
                ),
                RuntimeTaskLane::Interactive,
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

    fn starts_session(&self) -> bool {
        matches!(
            self,
            Self::LoadStoredToken
                | Self::StartOAuth { .. }
                | Self::StartBrowserSession { .. }
                | Self::SignOut
                | Self::Disconnect
        )
    }

    fn navigation_slot(&self) -> Option<NavigationSlot> {
        self.descriptor().navigation_slot
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
    UserGroupsDiscovered(Vec<SlackUserGroup>),
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
    /// Fresh `conversations.info` record for the details dialog.
    ConversationDetailsLoaded {
        channel_id: String,
        conversation: Box<SlackConversation>,
    },
    /// One page of member IDs for the details dialog.
    ConversationMembersLoaded {
        channel_id: String,
        user_ids: Vec<String>,
        complete: bool,
    },
    /// Conversation record re-read after a topic or purpose edit.
    ConversationDetailsUpdated {
        channel_id: String,
        conversation: Box<SlackConversation>,
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
    /// One followed-threads page; its roots already reached the thread catalog.
    FollowedThreadsLoaded {
        keys: Vec<(String, String)>,
        next_cursor: Option<String>,
        append: bool,
    },
    UserProfileLoadCompleted {
        user_id: String,
    },
    EmojiCatalogLoaded(HashMap<String, String>),
    WorkspaceThemeLoaded(crate::slack::SidebarTheme),
    /// Workspace icon URL from `team.info`, shown at the top of the navigation rail.
    WorkspaceIconLoaded(String),
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
    SlashCommandExecuted {
        channel_id: String,
        command: String,
        thread_ts: Option<String>,
        response: Box<crate::slack::SlashCommandResponse>,
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
            Self::ConversationChannelsDiscovered(_)
            | Self::ConversationPeopleDiscovered(_)
            | Self::UserGroupsDiscovered(_) => OperationContext::new(
                RuntimeOperation::ConversationDiscovery,
                RuntimeTarget::Workspace,
            ),
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
            Self::ConversationDetailsLoaded { channel_id, .. }
            | Self::ConversationMembersLoaded { channel_id, .. } => OperationContext::new(
                RuntimeOperation::ConversationDetails,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::ConversationDetailsUpdated { channel_id, .. } => OperationContext::new(
                RuntimeOperation::ConversationEdit,
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
            Self::FollowedThreadsLoaded { .. } => {
                OperationContext::new(RuntimeOperation::FollowedThreads, RuntimeTarget::Workspace)
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
            | Self::SlashCommandExecuted { .. }
            | Self::MessageUpdateCompleted { .. }
            | Self::ReactionUpdateCompleted { .. }
            | Self::SavedUpdated { .. }
            | Self::MessageActionCompleted { .. }
            | Self::MessageActionFailed { .. }
            | Self::AttachmentDownloadProgress { .. }
            | Self::FileUploadProgress { .. }
            | Self::FileUploaded(_)
            | Self::WorkspaceThemeLoaded(_)
            | Self::WorkspaceIconLoaded(_) => fallback.clone(),
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

struct RuntimeRequest {
    identity: RuntimeIdentity,
    command: RuntimeCommand,
    descriptor: RuntimeCommandDescriptor,
    in_flight_key: Option<RuntimeInFlightKey>,
    starts_session: bool,
}

impl RuntimeRequest {
    fn new(identity: RuntimeIdentity, command: RuntimeCommand) -> Self {
        let descriptor = command.descriptor();
        let in_flight_key = match descriptor.admission.kind {
            RuntimeAdmissionKind::Coalescible => Some(RuntimeInFlightKey::Coalescible {
                session: identity.session,
                key: descriptor
                    .admission
                    .replacement_key
                    .expect("coalescible command requires a replacement key"),
            }),
            RuntimeAdmissionKind::DurableAction => {
                let context = if matches!(&command, RuntimeCommand::Huddle(_)) {
                    OperationContext::new(
                        RuntimeOperation::Huddle,
                        RuntimeTarget::Huddle("active".to_string()),
                    )
                } else {
                    descriptor.context.clone()
                };
                Some(RuntimeInFlightKey::Ordered {
                    session: identity.session,
                    context,
                })
            }
            RuntimeAdmissionKind::Control | RuntimeAdmissionKind::Supersedable => None,
        };
        Self {
            identity,
            command,
            descriptor,
            in_flight_key,
            starts_session: false,
        }
    }
}

impl std::fmt::Debug for RuntimeRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeRequest")
            .field("identity", &self.identity)
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

mod admission;
use self::admission::*;

#[derive(Clone, Debug)]
pub(crate) enum SyncJobPayload {
    WorkspaceStartup,
    WorkspaceRefresh,
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
    message_handoffs: Arc<Mutex<MessageHandoffResolver>>,
    conversation_star_sync: ConversationStarSyncGate,
    user_status_sync: UserStatusSync,
    team_id: Option<String>,
    huddles: HuddleActorHandle,
    read_flusher: ReadFlusherHandle,
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
    read_flusher: Option<ReadFlusherHandle>,
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

    fn conversation(&self, channel_id: &str) -> Option<SlackConversation> {
        self.coordinator
            .lock()
            .expect("workspace coordinator lock poisoned")
            .conversation(channel_id)
            .cloned()
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
        if let Some(reduction) = reduction.as_ref() {
            self.dispatch_read_marks(reduction);
        }
        reduction
    }

    fn with_read_flusher(mut self, read_flusher: ReadFlusherHandle) -> Self {
        self.read_flusher = Some(read_flusher);
        self
    }

    /// Hands ledger marks the reducer made on its own (posting reads the
    /// conversation) to the flusher, so Slack agrees with every window.
    fn dispatch_read_marks(&self, reduction: &WorkspaceReduction) {
        let Some(read_flusher) = self.read_flusher.as_ref() else {
            return;
        };
        for effect in reduction.effects() {
            if let WorkspaceEffect::ConversationReadMarked { channel_id, ts } = effect {
                read_flusher.enqueue(&ReadTarget::conversation(channel_id), ts);
            }
        }
    }

    fn observe_reduction(&self, origin: MutationOrigin, reduction: Option<&WorkspaceReduction>) {
        if let Some(reduction) = reduction {
            let revision = reduction.patch().revision().value();
            for effect in reduction.effects() {
                let WorkspaceEffect::MessageAttention(effect) = effect else {
                    continue;
                };
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
    /// pending mutation cannot make the classification stale.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HuddleObservationKind {
    Discovered,
    Ended,
    Presence,
}

fn huddle_observation_kind(input: &CoordinatorInput) -> Option<HuddleObservationKind> {
    match input {
        CoordinatorInput::HuddleDiscovered(_) => Some(HuddleObservationKind::Discovered),
        CoordinatorInput::HuddleEnded { .. } => Some(HuddleObservationKind::Ended),
        CoordinatorInput::PresenceChanged { .. } => Some(HuddleObservationKind::Presence),
        CoordinatorInput::OpenPreflight { .. }
        | CoordinatorInput::JoinRequested { .. }
        | CoordinatorInput::OpenExternally { .. }
        | CoordinatorInput::LeaveRequested
        | CoordinatorInput::Dismissed
        | CoordinatorInput::MutedChanged(_)
        | CoordinatorInput::CameraChanged(_)
        | CoordinatorInput::ScreenShareChanged(_)
        | CoordinatorInput::ScreenShareStarted
        | CoordinatorInput::ScreenShareStopped
        | CoordinatorInput::ScreenShareFailed(_)
        | CoordinatorInput::DeviceSelected { .. }
        | CoordinatorInput::MediaConnected
        | CoordinatorInput::ConnectionLost
        | CoordinatorInput::MediaReconnected
        | CoordinatorInput::MediaStopped
        | CoordinatorInput::StatisticsUpdated(_)
        | CoordinatorInput::JoinCapabilityChanged(_)
        | CoordinatorInput::Failed(_)
        | CoordinatorInput::Reset => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HuddleActorMessageKind {
    Command,
    Observation(HuddleObservationKind),
}

enum HuddleActorMessage {
    Command {
        command: HuddleCommand,
        completion: oneshot::Sender<Result<()>>,
    },
    Observation {
        input: CoordinatorInput,
        kind: HuddleObservationKind,
        observation_slot: OwnedSemaphorePermit,
    },
}

impl HuddleActorMessage {
    fn kind(&self) -> HuddleActorMessageKind {
        match self {
            Self::Command { .. } => HuddleActorMessageKind::Command,
            Self::Observation { kind, .. } => HuddleActorMessageKind::Observation(*kind),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HuddleActorAdmissionSnapshot {
    admitted: u64,
    admitted_commands: u64,
    admitted_observations: u64,
    dequeued: u64,
    blocked: u64,
    closed: u64,
    depth: usize,
    observation_depth: usize,
    peak_depth: usize,
    peak_observation_depth: usize,
}

#[derive(Default)]
struct HuddleActorAdmissionMetrics {
    snapshot: HuddleActorAdmissionSnapshot,
}

impl HuddleActorAdmissionMetrics {
    fn record_admitted(&mut self, kind: HuddleActorMessageKind) {
        self.snapshot.admitted = self.snapshot.admitted.saturating_add(1);
        self.snapshot.depth = self.snapshot.depth.saturating_add(1);
        match kind {
            HuddleActorMessageKind::Command => {
                self.snapshot.admitted_commands = self.snapshot.admitted_commands.saturating_add(1);
            }
            HuddleActorMessageKind::Observation(_) => {
                self.snapshot.admitted_observations =
                    self.snapshot.admitted_observations.saturating_add(1);
                self.snapshot.observation_depth = self.snapshot.observation_depth.saturating_add(1);
            }
        }
        self.snapshot.peak_depth = self.snapshot.peak_depth.max(self.snapshot.depth);
        self.snapshot.peak_observation_depth = self
            .snapshot
            .peak_observation_depth
            .max(self.snapshot.observation_depth);
    }

    fn record_dequeued(&mut self, kind: HuddleActorMessageKind) {
        self.snapshot.dequeued = self.snapshot.dequeued.saturating_add(1);
        self.snapshot.depth = self.snapshot.depth.saturating_sub(1);
        if matches!(kind, HuddleActorMessageKind::Observation(_)) {
            self.snapshot.observation_depth = self.snapshot.observation_depth.saturating_sub(1);
        }
    }

    fn record_closed(&mut self) {
        self.snapshot.closed = self.snapshot.closed.saturating_add(1);
    }

    fn record_blocked(&mut self) {
        self.snapshot.blocked = self.snapshot.blocked.saturating_add(1);
    }
}

struct HuddleActorAdmission {
    observation_slots: Arc<Semaphore>,
    metrics: Mutex<HuddleActorAdmissionMetrics>,
}

impl HuddleActorAdmission {
    fn snapshot(&self) -> HuddleActorAdmissionSnapshot {
        self.metrics
            .lock()
            .expect("huddle actor admission metrics lock poisoned")
            .snapshot
    }

    fn record_closed(&self) {
        self.metrics
            .lock()
            .expect("huddle actor admission metrics lock poisoned")
            .record_closed();
    }

    fn record_blocked(&self) {
        self.metrics
            .lock()
            .expect("huddle actor admission metrics lock poisoned")
            .record_blocked();
    }
}

#[derive(Clone)]
struct HuddleActorHandle {
    sender: mpsc::Sender<HuddleActorMessage>,
    admission: Arc<HuddleActorAdmission>,
}

impl std::fmt::Debug for HuddleActorHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HuddleActorHandle").finish()
    }
}

impl HuddleActorHandle {
    async fn admit(&self, message: HuddleActorMessage, blocked: bool) -> Result<()> {
        let kind = message.kind();
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !blocked {
                    self.admission.record_blocked();
                }
                match self.sender.reserve().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        self.admission.record_closed();
                        return Err(anyhow!("huddle coordinator is not available"));
                    }
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.admission.record_closed();
                return Err(anyhow!("huddle coordinator is not available"));
            }
        };
        let mut metrics = self
            .admission
            .metrics
            .lock()
            .expect("huddle actor admission metrics lock poisoned");
        permit.send(message);
        metrics.record_admitted(kind);
        Ok(())
    }

    async fn command(&self, command: HuddleCommand) -> Result<()> {
        let completed = self.admit_command(command).await?;
        completed
            .await
            .map_err(|_| anyhow!("huddle coordinator stopped before completing command"))?
    }

    async fn admit_command(&self, command: HuddleCommand) -> Result<oneshot::Receiver<Result<()>>> {
        let (completion, completed) = oneshot::channel();
        self.admit(
            HuddleActorMessage::Command {
                command,
                completion,
            },
            false,
        )
        .await?;
        Ok(completed)
    }

    async fn observe(&self, input: CoordinatorInput) -> Result<()> {
        let Some(kind) = huddle_observation_kind(&input) else {
            return Err(anyhow!("huddle actor input is not an observation"));
        };
        let observation_slots = Arc::clone(&self.admission.observation_slots);
        let (observation_slot, blocked) = match Arc::clone(&observation_slots).try_acquire_owned() {
            Ok(slot) => (slot, false),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                self.admission.record_blocked();
                match observation_slots.acquire_owned().await {
                    Ok(slot) => (slot, true),
                    Err(_) => {
                        self.admission.record_closed();
                        return Err(anyhow!("huddle coordinator is not available"));
                    }
                }
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                self.admission.record_closed();
                return Err(anyhow!("huddle coordinator is not available"));
            }
        };
        self.admit(
            HuddleActorMessage::Observation {
                input,
                kind,
                observation_slot,
            },
            blocked,
        )
        .await
    }

    async fn observe_huddle(&self, huddle: ActiveHuddle) -> Result<()> {
        self.observe(CoordinatorInput::HuddleDiscovered(huddle))
            .await
    }

    #[cfg(test)]
    fn snapshot(&self) -> HuddleActorAdmissionSnapshot {
        self.admission.snapshot()
    }
}

struct HuddleActorReceiver {
    receiver: mpsc::Receiver<HuddleActorMessage>,
    admission: Arc<HuddleActorAdmission>,
}

impl HuddleActorReceiver {
    async fn recv(&mut self) -> Option<HuddleActorMessage> {
        std::future::poll_fn(|context| {
            let mut metrics = self
                .admission
                .metrics
                .lock()
                .expect("huddle actor admission metrics lock poisoned");
            match std::pin::Pin::new(&mut self.receiver).poll_recv(context) {
                std::task::Poll::Ready(Some(message)) => {
                    metrics.record_dequeued(message.kind());
                    std::task::Poll::Ready(Some(message))
                }
                std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
    }

    fn close(&mut self) {
        self.admission.observation_slots.close();
        self.receiver.close();
    }

    fn snapshot(&self) -> HuddleActorAdmissionSnapshot {
        self.admission.snapshot()
    }
}

fn huddle_actor_channel() -> (HuddleActorHandle, HuddleActorReceiver) {
    huddle_actor_channel_with_capacity(
        HUDDLE_ACTOR_QUEUE_CAPACITY,
        HUDDLE_ACTOR_OBSERVATION_CAPACITY,
    )
}

fn huddle_actor_channel_with_capacity(
    capacity: usize,
    observation_capacity: usize,
) -> (HuddleActorHandle, HuddleActorReceiver) {
    assert!(capacity > 0, "huddle actor capacity must be positive");
    assert!(
        observation_capacity <= capacity,
        "huddle observation capacity exceeds total capacity"
    );
    let (sender, receiver) = mpsc::channel(capacity);
    let admission = Arc::new(HuddleActorAdmission {
        observation_slots: Arc::new(Semaphore::new(observation_capacity)),
        metrics: Mutex::new(HuddleActorAdmissionMetrics::default()),
    });
    (
        HuddleActorHandle {
            sender,
            admission: Arc::clone(&admission),
        },
        HuddleActorReceiver {
            receiver,
            admission,
        },
    )
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

struct HuddleSessionSupervisor {
    session: SessionId,
    start: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    admission: Arc<HuddleActorAdmission>,
}

impl HuddleSessionSupervisor {
    fn spawn(
        session: SessionId,
        receiver: HuddleActorReceiver,
        events: RuntimeEventSender,
        join_capability: NativeJoinCapability,
    ) -> Self {
        let admission = Arc::clone(&receiver.admission);
        let (start, start_receiver) = oneshot::channel();
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            if start_receiver.await.is_err() {
                reject_unstarted_huddle_actor(receiver).await;
                return;
            }
            run_huddle_actor(receiver, events, join_capability, shutdown_receiver).await;
        });
        Self {
            session,
            start: Some(start),
            shutdown: Some(shutdown),
            task,
            admission,
        }
    }

    fn start(&mut self) -> bool {
        self.start
            .take()
            .is_some_and(|start| start.send(()).is_ok())
    }

    async fn shutdown(mut self) {
        self.admission.observation_slots.close();
        drop(self.start.take());
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Err(error) = self.task.await {
            crate::debug::log(
                "huddle",
                &format!(
                    "HuddleSupervisorFailed session={:?} error={error}",
                    self.session
                ),
            );
        }
        let snapshot = self.admission.snapshot();
        crate::debug::log(
            "huddle",
            &format!(
                "HuddleActorAdmissionSummary admitted={} commands={} observations={} dequeued={} blocked={} closed={} depth={} observation_depth={} peak={} observation_peak={}",
                snapshot.admitted,
                snapshot.admitted_commands,
                snapshot.admitted_observations,
                snapshot.dequeued,
                snapshot.blocked,
                snapshot.closed,
                snapshot.depth,
                snapshot.observation_depth,
                snapshot.peak_depth,
                snapshot.peak_observation_depth,
            ),
        );
    }
}

struct ReadFlusherSessionSupervisor {
    session: SessionId,
    start: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ReadFlusherSessionSupervisor {
    fn spawn(
        session: SessionId,
        handle: ReadFlusherHandle,
        api: SlackApi,
        debounce_delay: Duration,
    ) -> Self {
        let (start, start_receiver) = oneshot::channel();
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            if start_receiver.await.is_err() {
                return;
            }
            run_read_flusher_loop(handle, api, debounce_delay, shutdown_receiver).await;
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
                "read_flusher",
                &format!(
                    "ReadFlusherSupervisorFailed session={:?} error={error}",
                    self.session
                ),
            );
        }
    }
}

#[derive(Default)]
struct RuntimeSessionSupervisors {
    realtime: Option<RealtimeSessionSupervisor>,
    huddle: Option<HuddleSessionSupervisor>,
    read_flusher: Option<ReadFlusherSessionSupervisor>,
}

struct RuntimeState {
    active_session: SessionId,
    connection: Option<RuntimeConnection>,
    realtime: Option<RealtimeSessionSupervisor>,
    huddle: Option<HuddleSessionSupervisor>,
    read_flusher: Option<ReadFlusherSessionSupervisor>,
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
            huddle: None,
            read_flusher: None,
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

    fn begin_session_replacement(&mut self, session: SessionId) -> RuntimeSessionSupervisors {
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
        RuntimeSessionSupervisors {
            realtime: self.realtime.take(),
            huddle: self.huddle.take(),
            read_flusher: self.read_flusher.take(),
        }
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

    fn install_huddle_supervisor(
        &mut self,
        mut supervisor: HuddleSessionSupervisor,
    ) -> std::result::Result<(), HuddleSessionSupervisor> {
        if self.active_session != supervisor.session || self.huddle.is_some() {
            return Err(supervisor);
        }
        if !supervisor.start() {
            return Err(supervisor);
        }
        self.huddle = Some(supervisor);
        Ok(())
    }

    fn install_read_flusher_supervisor(
        &mut self,
        mut supervisor: ReadFlusherSessionSupervisor,
    ) -> std::result::Result<(), ReadFlusherSessionSupervisor> {
        if self.active_session != supervisor.session || self.read_flusher.is_some() {
            return Err(supervisor);
        }
        if !supervisor.start() {
            return Err(supervisor);
        }
        self.read_flusher = Some(supervisor);
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
    let supervisors = state
        .lock()
        .expect("runtime state lock poisoned")
        .begin_session_replacement(session);
    if let Some(realtime) = supervisors.realtime {
        realtime.shutdown().await;
    }
    if let Some(huddle) = supervisors.huddle {
        huddle.shutdown().await;
    }
    if let Some(flusher) = supervisors.read_flusher {
        flusher.shutdown().await;
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
    commands: RuntimeCommandSender,
}

pub(crate) struct RuntimeCommandRejection {
    pub(crate) context: OperationContext,
    pub(crate) command: RuntimeCommand,
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

    #[cfg(test)]
    pub(crate) fn content_type(&self) -> &'static str {
        self.mime_type.as_str()
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    #[cfg(test)]
    pub(crate) fn is_video(&self) -> bool {
        self.mime_type.is_video()
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

struct RuntimeReadyRequest {
    request: RuntimeRequest,
    lease: Option<RuntimeCommandLease>,
    session_start: Option<RuntimeSessionStartReservation>,
}

struct RuntimeReadyReceivers {
    control: Option<mpsc::Receiver<RuntimeReadyRequest>>,
    lanes: [Option<mpsc::Receiver<RuntimeReadyRequest>>; 5],
    next_lane: usize,
    control_budget: usize,
}

impl RuntimeReadyReceivers {
    async fn recv(&mut self) -> Option<RuntimeReadyRequest> {
        std::future::poll_fn(|context| {
            let mut open = false;
            let control_first = self.control_budget > 0;
            if control_first {
                if let Some(receiver) = self.control.as_mut() {
                    match std::pin::Pin::new(receiver).poll_recv(context) {
                        std::task::Poll::Ready(Some(request)) => {
                            self.control_budget -= 1;
                            return std::task::Poll::Ready(Some(request));
                        }
                        std::task::Poll::Ready(None) => self.control = None,
                        std::task::Poll::Pending => open = true,
                    }
                }
            }
            for offset in 0..RuntimeTaskLane::ALL.len() {
                let lane_position = (self.next_lane + offset) % RuntimeTaskLane::ALL.len();
                let lane = RuntimeTaskLane::ALL[lane_position];
                let index = lane.index();
                if let Some(receiver) = self.lanes[index].as_mut() {
                    match std::pin::Pin::new(receiver).poll_recv(context) {
                        std::task::Poll::Ready(Some(request)) => {
                            self.next_lane = (lane_position + 1) % RuntimeTaskLane::ALL.len();
                            self.control_budget = RUNTIME_CONTROL_DISPATCH_BURST;
                            return std::task::Poll::Ready(Some(request));
                        }
                        std::task::Poll::Ready(None) => self.lanes[index] = None,
                        std::task::Poll::Pending => open = true,
                    }
                }
            }
            if !control_first {
                if let Some(receiver) = self.control.as_mut() {
                    match std::pin::Pin::new(receiver).poll_recv(context) {
                        std::task::Poll::Ready(Some(request)) => {
                            return std::task::Poll::Ready(Some(request));
                        }
                        std::task::Poll::Ready(None) => self.control = None,
                        std::task::Poll::Pending => open = true,
                    }
                }
            }
            if open {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(None)
            }
        })
        .await
    }
}

fn spawn_runtime_command_drivers(
    commands: &RuntimeCommandReceiver,
    limits: &RuntimeTaskLimits,
) -> RuntimeReadyReceivers {
    let (control_sender, control) = mpsc::channel(1);
    let control_commands = commands.clone();
    std::mem::drop(tokio::spawn(async move {
        while let Some(control_request) = control_commands.recv_control().await {
            let RuntimeControlRequest {
                request,
                session_start,
            } = control_request;
            if control_sender
                .send(RuntimeReadyRequest {
                    request,
                    lease: None,
                    session_start,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }));

    let mut receivers: [Option<mpsc::Receiver<RuntimeReadyRequest>>; 5] =
        std::array::from_fn(|_| None);
    for lane in RuntimeTaskLane::ALL {
        let (sender, receiver) = mpsc::channel(lane.running_capacity());
        receivers[lane.index()] = Some(receiver);
        let lane_commands = commands.clone();
        let lane_limits = limits.clone();
        std::mem::drop(tokio::spawn(async move {
            while let Some(lane_request) = lane_commands.recv_lane(lane).await {
                let RuntimeLaneRequest {
                    request,
                    reservation,
                } = lane_request;
                let lease =
                    if request.descriptor.admission.kind == RuntimeAdmissionKind::Supersedable {
                        debug_assert!(reservation.is_none());
                        None
                    } else {
                        Some(RuntimeCommandLease {
                            _permit: lane_limits.acquire(lane).await,
                            _reservation: reservation
                                .expect("admitted command requires an in-flight reservation"),
                        })
                    };
                if sender
                    .send(RuntimeReadyRequest {
                        request,
                        lease,
                        session_start: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    RuntimeReadyReceivers {
        control: Some(control),
        lanes: receivers,
        next_lane: 0,
        control_budget: RUNTIME_CONTROL_DISPATCH_BURST,
    }
}

impl AppRuntime {
    pub fn start() -> (Self, RuntimeEventReceiver) {
        let (commands, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::production());
        let (events, event_receiver) = runtime_event_channel();

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

    pub(crate) fn send(
        &self,
        identity: RuntimeIdentity,
        command: RuntimeCommand,
    ) -> std::result::Result<(), Box<RuntimeCommandRejection>> {
        match self
            .commands
            .try_send(RuntimeRequest::new(identity, command))
        {
            Ok(_) => Ok(()),
            Err(error) if error.outcome == RuntimeCommandAdmissionOutcome::DroppedStale => Ok(()),
            Err(error) => {
                crate::debug::log(
                    "runtime",
                    &format!(
                        "RuntimeCommandRejected outcome={:?} session={:?} request={:?} operation={:?} admission={:?}",
                        error.outcome,
                        error.request.identity.session,
                        error.request.identity.request,
                        error.request.descriptor.context.operation,
                        error.request.descriptor.admission.kind,
                    ),
                );
                let RuntimeRequest {
                    command,
                    descriptor,
                    ..
                } = error.request;
                Err(Box::new(RuntimeCommandRejection {
                    context: descriptor.context,
                    command,
                }))
            }
        }
    }
}

async fn run_runtime(commands: RuntimeCommandReceiver, events: RuntimeEventMailboxSender) {
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
    let admission_limits = RuntimeTaskLimits::new(
        RUNTIME_NAVIGATION_TASK_CAPACITY,
        RUNTIME_INTERACTIVE_TASK_CAPACITY,
        RUNTIME_BACKGROUND_TASK_CAPACITY,
        RUNTIME_IMAGE_TASK_CAPACITY,
        RUNTIME_UPLOAD_TASK_CAPACITY,
    );
    let mut ready = spawn_runtime_command_drivers(&commands, &admission_limits);

    while let Some(ready_request) = ready.recv().await {
        let RuntimeReadyRequest {
            request,
            lease,
            session_start,
        } = ready_request;
        let identity = request.identity;
        let trace_fields = RuntimeTraceFields::for_descriptor(identity, &request.descriptor);
        let span = trace_fields.span();
        let _entered = span.enter();
        let active_session = state
            .lock()
            .expect("runtime state lock poisoned")
            .active_session;
        if identity.session < active_session {
            drop(session_start);
            continue;
        }
        if identity.session > active_session {
            replace_session_and_drain(&state, identity.session).await;
        }

        let event_sender =
            RuntimeEventSender::new(events.clone(), identity, request.descriptor.context.clone());
        dispatch_command(
            request,
            event_sender,
            RuntimeDispatchResources {
                state: &state,
                oauth: &oauth,
                image_cache: &image_cache,
                limits: &limits,
            },
            lease,
        );
        drop(session_start);
    }

    admission_limits.wait_idle().await;
    let admission = commands.snapshot();
    crate::debug::log(
        "runtime",
        &format!(
            "RuntimeCommandAdmissionClosed admitted={} dequeued={} coalesced={} superseded={} cancelled={} rejected={} depth={} peak={} in_flight={} peak_in_flight={}",
            admission.admitted,
            admission.dequeued,
            admission.coalesced,
            admission.superseded,
            admission.cancelled,
            admission.rejected,
            admission.depth,
            admission.peak_depth,
            admission.in_flight,
            admission.peak_in_flight,
        ),
    );
    replace_session_and_drain(&state, SessionId::default()).await;
    let event_metrics = events.snapshot();
    crate::debug::log(
        "runtime",
        &format!(
            "RuntimeEventMailboxSummary admitted={} dequeued={} blocked={} closed={} depth={} peak={} coalesced_progress={} dropped_progress={}",
            event_metrics.admitted,
            event_metrics.dequeued,
            event_metrics.blocked,
            event_metrics.closed,
            event_metrics.depth,
            event_metrics.peak_depth,
            event_metrics.coalesced_progress,
            event_metrics.dropped_progress,
        ),
    );
}

struct RuntimeDispatchResources<'a> {
    state: &'a Arc<Mutex<RuntimeState>>,
    oauth: &'a SlackOAuthClient,
    image_cache: &'a ImageAssetCache,
    limits: &'a RuntimeTaskLimits,
}

fn dispatch_command(
    request: RuntimeRequest,
    events: RuntimeEventSender,
    resources: RuntimeDispatchResources<'_>,
    lease: Option<RuntimeCommandLease>,
) {
    let RuntimeRequest {
        identity,
        command,
        descriptor,
        ..
    } = request;
    let RuntimeDispatchResources {
        state,
        oauth,
        image_cache,
        limits,
    } = resources;
    let lane = descriptor.lane;
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
                lease,
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
                lease,
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
                lease,
                AuthenticationFailureContext::BrowserSession,
                authenticate_token(token),
            );
        }
        RuntimeCommand::SignOut => {
            debug_assert!(lease.is_none());
            finish_sign_out(&events, TokenStore.clear());
        }
        RuntimeCommand::Disconnect => {
            debug_assert!(lease.is_none());
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::SignedOut,
            ));
        }
        RuntimeCommand::UpdateAttentionPreferences(preferences) => {
            let _lease = lease.expect("attention preference command requires an admission lease");
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
            let tracked_request = TrackedRequest::for_command(identity, &command);
            let image_cache = image_cache.clone();
            let execution_limits = limits.clone();
            spawn_request_task(state, tracked_request, async move {
                let _lease = lease;
                let _execution_permit = execution_limits.acquire(lane).await;
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
    lease: Option<RuntimeCommandLease>,
    failure_context: AuthenticationFailureContext,
    future: F,
) where
    F: Future<Output = Result<(StoredToken, SlackApi, AuthInfo)>> + Send + 'static,
{
    let state_for_task = Arc::clone(state);
    let execution_limits = limits.clone();
    spawn_request_task(
        state,
        TrackedRequest::new(
            identity,
            OperationContext::new(RuntimeOperation::Authenticate, RuntimeTarget::Workspace),
        ),
        async move {
            let _lease = lease;
            let _execution_permit = execution_limits.acquire(RuntimeTaskLane::Interactive).await;
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
                        let read_flusher = ReadFlusherHandle::new(
                            Arc::new(Mutex::new(ReadFlusherQueue::new())),
                            Arc::new(Notify::new()),
                        );
                        let workspace = WorkspaceReducerAdapter::default()
                            .with_read_flusher(read_flusher.clone());
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
                            message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(
                                256,
                            ))),
                            conversation_star_sync: ConversationStarSyncGate::default(),
                            user_status_sync: UserStatusSync::default(),
                            team_id: auth.team_id.clone(),
                            huddles,
                            read_flusher,
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
                    let icon_api = connection.slack.clone();
                    let icon_events = events.clone();
                    tokio::spawn(async move {
                        match icon_api.team_icon_url().await {
                            Ok(Some(url)) => {
                                icon_events.send_event(RuntimeEventKind::WorkspaceIconLoaded(url));
                            }
                            Ok(None) => {}
                            Err(error) => crate::debug::log(
                                "runtime",
                                &format!("WorkspaceIconLoadFailed error={error:#}"),
                            ),
                        }
                    });
                    let theme_api = connection.slack.clone();
                    let theme_events = events.clone();
                    let theme_store = connection.workspace_store.clone();
                    tokio::spawn(async move {
                        match theme_api.fetch_sidebar_theme().await {
                            Ok(Some(theme)) => {
                                if let Some(store) = theme_store.as_ref() {
                                    if let Err(error) = store.store_sidebar_theme(&theme).await {
                                        crate::debug::log(
                                            "store",
                                            &format!("SidebarThemePersistFailed error={error:#}"),
                                        );
                                    }
                                }
                                theme_events
                                    .send_event(RuntimeEventKind::WorkspaceThemeLoaded(theme));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                crate::debug::log(
                                    "runtime",
                                    &format!("SidebarThemeLoadFailed error={error:#}"),
                                );
                            }
                        }
                    });
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
    huddle_receiver: HuddleActorReceiver,
) {
    let huddle_events = events.unsolicited(OperationContext::new(
        RuntimeOperation::Huddle,
        RuntimeTarget::Huddle("active".to_string()),
    ));
    let huddle_supervisor = HuddleSessionSupervisor::spawn(
        identity.session,
        huddle_receiver,
        huddle_events,
        production_native_join_capability(connection.slack.browser_cookie_d().is_some()),
    );
    let rejected_huddle = state
        .lock()
        .expect("runtime state lock poisoned")
        .install_huddle_supervisor(huddle_supervisor)
        .err();
    if let Some(rejected_huddle) = rejected_huddle {
        rejected_huddle.shutdown().await;
        return;
    }

    let flusher_supervisor = ReadFlusherSessionSupervisor::spawn(
        identity.session,
        connection.read_flusher.clone(),
        connection.slack.clone(),
        Duration::from_millis(500),
    );
    let rejected_flusher = state
        .lock()
        .expect("runtime state lock poisoned")
        .install_read_flusher_supervisor(flusher_supervisor)
        .err();
    if let Some(rejected_flusher) = rejected_flusher {
        rejected_flusher.shutdown().await;
        return;
    }

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

fn sync_job_cancellation_id(_payload: &SyncJobPayload, job_sequence: u64) -> CancellationId {
    CancellationId::new(job_sequence)
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
        Ok(AdmissionOutcome::Accepted { coalesced, .. }) => {
            let mut pending_jobs = connection.pending_jobs.lock().unwrap();
            if let Some(released) = coalesced {
                pending_jobs.remove(&released.job().id());
            }
            pending_jobs.insert(job_id, payload);
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

fn sync_job_outcome_for_error(error: &anyhow::Error) -> JobOutcome {
    let is_retryable = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SlackError>())
        .is_some_and(|slack_error| {
            matches!(
                slack_error.category(),
                SlackErrorCategory::Connectivity | SlackErrorCategory::RateLimited
            )
        });
    if is_retryable {
        JobOutcome::RetryableFailure
    } else {
        JobOutcome::PermanentFailure
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
                        let outcome = sync_job_outcome_for_error(&error);
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "SyncJobFailed job_id={:?} outcome={:?} error={error:#}",
                                job_id, outcome
                            ),
                        );
                        outcome
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
                    Ok(
                        CompletionOutcome::Completed
                        | CompletionOutcome::Failed(_)
                        | CompletionOutcome::Cancelled(_)
                        | CompletionOutcome::Superseded { .. },
                    ) => {
                        connection_clone
                            .pending_jobs
                            .lock()
                            .unwrap()
                            .remove(&job_id);
                    }
                    Ok(CompletionOutcome::Retried { ready_at_ms, .. }) => {
                        let retry_state = Arc::clone(&state_clone);
                        let retry_connection = connection_clone.clone();
                        let retry_limits = limits.clone();
                        let retry_events = events_clone.clone();
                        spawn_session_task(&state_clone, identity.session, async move {
                            loop {
                                let current_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                                    as u64;
                                if current_ms >= ready_at_ms {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(
                                    ready_at_ms.saturating_sub(current_ms).max(1),
                                ))
                                .await;
                            }
                            drive_scheduler(
                                &retry_state,
                                identity,
                                retry_connection,
                                retry_limits,
                                retry_events,
                            );
                        });
                    }
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

            sync_custom_emojis_best_effort(events, connection).await;
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

/// Best-effort live refresh of the workspace's custom emoji catalog. Runs
/// alongside the main workspace refresh job rather than as its own job,
/// since the catalog changes rarely and doesn't warrant a separate schedule.
/// Never fails the caller: `emoji.list` errors are logged and swallowed so a
/// transient failure here can't take down the rest of workspace sync.
async fn sync_custom_emojis_best_effort(
    events: &RuntimeEventSender,
    connection: &RuntimeConnection,
) {
    let Some(store) = connection.workspace_store.as_ref() else {
        return;
    };
    match connection.slack.custom_emojis().await {
        Ok(emojis) => {
            if let Err(error) = store.store_custom_emojis(&emojis).await {
                crate::debug::log("store", &format!("CustomEmojiStoreFailed error={error:#}"));
            }
            if !emojis.is_empty() {
                events.send_event(RuntimeEventKind::EmojiCatalogLoaded(emojis));
            }
        }
        Err(error) => {
            crate::debug::log(
                "runtime",
                &format!("CustomEmojiFetchFailed error={error:#}"),
            );
        }
    }
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
        sidebar_theme,
        read_states,
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
    if let Some(theme) = sidebar_theme {
        events.send_event(RuntimeEventKind::WorkspaceThemeLoaded(theme));
    }
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
                read_states,
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
        RuntimeCommand::Huddle(command) => return connection.huddles.command(command).await,
        command => command,
    };
    let mut slack = Some(connection.slack.clone());
    let mut workspace_store = connection.workspace_store.clone();
    let mut user_cache = connection
        .user_cache
        .lock()
        .expect("runtime user cache lock poisoned")
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
        message_handoffs: &connection.message_handoffs,
        conversation_star_sync: &connection.conversation_star_sync,
        user_status_sync: &connection.user_status_sync,
        team_id: connection.team_id.as_deref(),
        workspace_url: connection.workspace_url.as_deref(),
        huddles: &connection.huddles,
        read_flusher: &connection.read_flusher,
    };

    let result = handle_command(command, &mut context).await;
    connection
        .user_cache
        .lock()
        .expect("runtime user cache lock poisoned")
        .extend(user_cache);
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
    message_handoffs: &'a Arc<Mutex<MessageHandoffResolver>>,
    conversation_star_sync: &'a ConversationStarSyncGate,
    user_status_sync: &'a UserStatusSync,
    team_id: Option<&'a str>,
    workspace_url: Option<&'a str>,
    huddles: &'a HuddleActorHandle,
    read_flusher: &'a ReadFlusherHandle,
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
    active_direct_message: bool,
    activity_score: f64,
    title: String,
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
            .then_with(|| right.active_direct_message.cmp(&left.active_direct_message))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    let (mut huddle_candidates, remaining): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|candidate| candidate.huddle_metadata);
    let (urgent_direct_messages, mut remaining): (Vec<_>, Vec<_>) = remaining
        .into_iter()
        .partition(|candidate| candidate.active_direct_message);
    huddle_candidates.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT);
    remaining.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT.saturating_sub(huddle_candidates.len()));
    huddle_candidates
        .into_iter()
        .chain(urgent_direct_messages)
        .chain(remaining)
        .map(|candidate| candidate.id)
        .collect()
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
    let active_direct_message = conversation.has_active_direct_message_hint();
    if !is_channel
        || ((conversation.is_im.unwrap_or(false) || conversation.is_mpim.unwrap_or(false))
            && !active_direct_message
            && !huddle_metadata)
    {
        return None;
    }

    Some(ChannelHistoryPrefetchCandidate {
        id: conversation.id.clone(),
        huddle_metadata,
        active_direct_message,
        activity_score: conversation_activity_score(conversation),
        title: conversation.display_name().to_lowercase(),
    })
}

fn conversation_activity_score(conversation: &SlackConversation) -> f64 {
    ["updated", "updated_at", "created", "latest", "latest_ts"]
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
            // Best-effort: tokens granted before `usergroups:read` was
            // requested get `missing_scope`, which must not block discovery.
            match api.user_groups().await {
                Ok(groups) => context
                    .events
                    .send_event(RuntimeEventKind::UserGroupsDiscovered(groups)),
                Err(error) => {
                    crate::debug::log("runtime", &format!("UserGroupsFetchFailed error={error:#}"))
                }
            }
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
        RuntimeCommand::LoadConversationDetails { channel_id } => {
            let api = require_slack(context.slack)?;
            let conversation = api.conversation_info(&channel_id).await?;
            context
                .events
                .send_event(RuntimeEventKind::ConversationDetailsLoaded {
                    channel_id: channel_id.clone(),
                    conversation: Box::new(conversation),
                });
            let mut cursor: Option<String> = None;
            let mut finished = false;
            for _ in 0..CONVERSATION_MEMBER_PAGE_LIMIT {
                let (user_ids, next) = api
                    .conversation_members_page(&channel_id, cursor.as_deref())
                    .await?;
                finished = next.is_none();
                context
                    .events
                    .send_event(RuntimeEventKind::ConversationMembersLoaded {
                        channel_id: channel_id.clone(),
                        user_ids,
                        complete: finished,
                    });
                if finished {
                    break;
                }
                cursor = next;
            }
            if !finished {
                // Very large conversation: treat the fetched pages as the list.
                context
                    .events
                    .send_event(RuntimeEventKind::ConversationMembersLoaded {
                        channel_id,
                        user_ids: Vec::new(),
                        complete: true,
                    });
            }
        }
        RuntimeCommand::SetConversationText {
            channel_id,
            field,
            text,
        } => {
            let api = require_slack(context.slack)?;
            match field {
                ConversationTextField::Topic => {
                    api.set_conversation_topic(&channel_id, &text).await?
                }
                ConversationTextField::Purpose => {
                    api.set_conversation_purpose(&channel_id, &text).await?
                }
            }
            let conversation = api.conversation_info(&channel_id).await?;
            context
                .events
                .send_event(RuntimeEventKind::ConversationDetailsUpdated {
                    channel_id,
                    conversation: Box::new(conversation),
                });
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
                    if let Err(error) = observe_huddle_messages(
                        context.huddles,
                        context.team_id,
                        &channel_id,
                        &messages,
                    )
                    .await
                    {
                        crate::debug::log(
                            "huddle",
                            &format!("HuddleCachedHistoryObservationFailed error={error}"),
                        );
                    }
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
            if let Err(error) = observe_huddle_messages(
                context.huddles,
                context.team_id,
                &channel_id,
                &page.messages,
            )
            .await
            {
                crate::debug::log(
                    "huddle",
                    &format!("HuddleHistoryObservationFailed error={error}"),
                );
            }
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
            if let Err(error) = observe_huddle_messages(
                context.huddles,
                context.team_id,
                &channel_id,
                &page.messages,
            )
            .await
            {
                crate::debug::log(
                    "huddle",
                    &format!("HuddleOlderHistoryObservationFailed error={error}"),
                );
            }
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
        RuntimeCommand::LoadFollowedThreads { cursor } => {
            let api = require_slack(context.slack)?;
            let page = api.followed_threads_page(cursor.as_deref()).await?;
            let keys = page
                .threads
                .iter()
                .map(|(channel_id, root)| (channel_id.clone(), root.ts.clone()))
                .collect();
            context
                .workspace
                .apply_persisted_and_publish_with_completion(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::FollowedThreadsObserved(page.threads),
                    RuntimeEventKind::FollowedThreadsLoaded {
                        keys,
                        next_cursor: page.next_cursor,
                        append: cursor.is_some(),
                    },
                )
                .await?;
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
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            blocks_json,
            attachments_json,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let mut message = api
                .post_message(
                    &channel_id,
                    &text,
                    blocks_json.as_deref(),
                    attachments_json.as_deref(),
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
        RuntimeCommand::ExecuteSlashCommand {
            channel_id,
            command,
            text,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let response = api
                .execute_slash_command(&channel_id, &command, &text, thread_ts.as_deref())
                .await?;
            context
                .events
                .send_event(RuntimeEventKind::SlashCommandExecuted {
                    channel_id,
                    command,
                    thread_ts,
                    response: Box::new(response),
                });
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
            let optimistic_applied = if let Some(user_id) = context.current_user_id {
                let reductions = context
                    .workspace
                    .apply_persisted_and_publish(
                        context.workspace_store.as_ref(),
                        context.events,
                        MutationOrigin::Local,
                        WorkspaceMutation::ReactionChanged {
                            channel_id: channel_id.clone(),
                            message_ts: ts.clone(),
                            name: name.clone(),
                            user_id: user_id.to_string(),
                            added: add,
                        },
                    )
                    .await?;
                !reductions.is_empty()
            } else {
                false
            };
            if let Err(error) = api.set_reaction(&channel_id, &ts, &name, add).await {
                if optimistic_applied {
                    if let Some(user_id) = context.current_user_id {
                        let _ = context
                            .workspace
                            .apply_persisted_and_publish(
                                context.workspace_store.as_ref(),
                                context.events,
                                MutationOrigin::Local,
                                WorkspaceMutation::ReactionChanged {
                                    channel_id: channel_id.clone(),
                                    message_ts: ts.clone(),
                                    name: name.clone(),
                                    user_id: user_id.to_string(),
                                    added: !add,
                                },
                            )
                            .await;
                    }
                }
                return Err(error.into());
            }
            let completion = RuntimeEventKind::ReactionUpdateCompleted {
                channel_id,
                message_ts: ts,
                thread_ts,
                projected: context.current_user_id.is_some(),
            };
            context.events.send_event(completion);
        }
        RuntimeCommand::SetSaved {
            channel_id,
            ts,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let previous_message = context.workspace.message(&channel_id, &ts);
            let optimistic_applied = if let Some(mut message) = previous_message.clone() {
                message.is_starred = Some(add);
                let reductions = context
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
                !reductions.is_empty()
            } else {
                false
            };
            if let Err(error) = api.set_saved(&channel_id, &ts, add).await {
                if optimistic_applied {
                    if let Some(message) = previous_message {
                        let _ = context
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
                            .await;
                    }
                }
                return Err(error.into());
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
            let previous_starred = context
                .workspace
                .conversation(&channel_id)
                .and_then(|c| c.is_starred);
            let reductions = context
                .workspace
                .apply_persisted_and_publish(
                    context.workspace_store.as_ref(),
                    context.events,
                    MutationOrigin::Local,
                    WorkspaceMutation::ConversationStarChanged {
                        channel_id: channel_id.clone(),
                        starred,
                    },
                )
                .await?;
            let optimistic_applied = !reductions.is_empty();
            if let Err(error) = api.set_conversation_starred(&channel_id, starred).await {
                if optimistic_applied {
                    let rollback_starred = previous_starred.unwrap_or(!starred);
                    let _ = context
                        .workspace
                        .apply_persisted_and_publish(
                            context.workspace_store.as_ref(),
                            context.events,
                            MutationOrigin::Local,
                            WorkspaceMutation::ConversationStarChanged {
                                channel_id: channel_id.clone(),
                                starred: rollback_starred,
                            },
                        )
                        .await;
                }
                return Err(error.into());
            }
            context
                .events
                .send_event(RuntimeEventKind::ConversationStarUpdateCompleted {
                    channel_id,
                    starred,
                });
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
        RuntimeCommand::MarkConversationRead { channel_id, ts } => {
            apply_local_read_state(
                context,
                WorkspaceMutation::ConversationMarked {
                    channel_id: channel_id.clone(),
                    ts: ts.clone(),
                },
            )
            .await;
            context
                .read_flusher
                .enqueue(&ReadTarget::conversation(channel_id), &ts);
        }
        RuntimeCommand::MarkConversationUnread { channel_id, ts } => {
            apply_local_read_state(
                context,
                WorkspaceMutation::ConversationMarkedUnread {
                    channel_id: channel_id.clone(),
                    ts: ts.clone(),
                },
            )
            .await;
            context
                .read_flusher
                .enqueue_force(&ReadTarget::conversation(channel_id), &ts);
        }
        RuntimeCommand::MarkThreadRead {
            channel_id,
            thread_ts,
            target_ts,
        } => {
            apply_local_read_state(
                context,
                WorkspaceMutation::ThreadRead {
                    channel_id: channel_id.clone(),
                    root_ts: thread_ts.clone(),
                },
            )
            .await;
            let target_ts = target_ts.unwrap_or_else(|| thread_ts.clone());
            context
                .read_flusher
                .enqueue(&ReadTarget::thread(channel_id, thread_ts), &target_ts);
        }
    }

    Ok(())
}

/// Applies a local read-state mutation through the coordinator so the
/// ledger, store and every window agree before Slack is told.
async fn apply_local_read_state(context: &RuntimeContext<'_>, mutation: WorkspaceMutation) {
    if let Err(error) = context
        .workspace
        .apply_persisted_and_publish(
            context.workspace_store.as_ref(),
            context.events,
            MutationOrigin::Local,
            mutation,
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!("LocalReadStateDeferred category={:?}", error.category()),
        );
    }
}

/// Fetches the `client.counts` baseline and feeds it to the coordinator.
/// Best effort: a failure keeps the previous baseline and live tier.
async fn sync_client_counts(
    events: &RuntimeEventSender,
    api: &SlackApi,
    store: Option<&WorkspaceStore>,
    workspace: &WorkspaceReducerAdapter,
) {
    let counts = match api.client_counts().await {
        Ok(counts) => counts,
        Err(error) => {
            crate::debug::log(
                "runtime",
                &format!("ClientCountsLoadFailed category={:?}", error.category()),
            );
            return;
        }
    };
    crate::debug::log(
        "runtime",
        &format!("ClientCountsLoaded conversations={}", counts.len()),
    );
    if let Err(error) = workspace
        .apply_persisted_and_publish(
            store,
            events,
            MutationOrigin::WebApi,
            WorkspaceMutation::CountsSnapshot(counts),
        )
        .await
    {
        crate::debug::log(
            "store",
            &format!("ClientCountsDeferred category={:?}", error.category()),
        );
    }
}

async fn run_socket_mode(
    credentials: socket_mode::SocketModeCredentials,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    mut shutdown: oneshot::Receiver<()>,
) {
    let RuntimeConnection {
        slack,
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
    // Bootstrap already loads `client.counts`; only reconnects re-snapshot.
    let connected_before = std::sync::atomic::AtomicBool::new(false);

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
            let connected_before = &connected_before;
            let counts_api = slack.clone();
            let counts_store = workspace_store.clone();
            let counts_workspace = workspace.clone();
            let run_once = socket_mode::run_once(
                &credentials,
                move || {
                    connected_events.send_event(RuntimeEventKind::RealtimeStatusChanged(
                        RealtimeStatus::online(transport),
                    ));
                    if connected_before.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        tokio::spawn(async move {
                            sync_client_counts(
                                &connected_events,
                                &counts_api,
                                counts_store.as_ref(),
                                &counts_workspace,
                            )
                            .await;
                        });
                    }
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
                        )
                        .await?;
                        let defer_workspace_mutation = persistence_for_event.is_some()
                            && matches!(
                                &event,
                                SocketModeEvent::Message(_)
                                    | SocketModeEvent::Reaction(_)
                                    | SocketModeEvent::UserChanged(_)
                                    | SocketModeEvent::UserHuddleChanged(_)
                                    | SocketModeEvent::ConversationMarked { .. }
                                    | SocketModeEvent::ThreadMarked { .. }
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
                            SocketModeEvent::Reaction(_)
                            | SocketModeEvent::ConversationMarked { .. }
                            | SocketModeEvent::ThreadMarked { .. } => {
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
        SocketModeEvent::ConversationMarked { channel_id, ts } => {
            Some(WorkspaceMutation::ConversationMarked {
                channel_id: channel_id.clone(),
                ts: ts.clone(),
            })
        }
        SocketModeEvent::ThreadMarked {
            channel_id,
            thread_ts,
        } => Some(WorkspaceMutation::ThreadRead {
            channel_id: channel_id.clone(),
            root_ts: thread_ts.clone(),
        }),
        SocketModeEvent::RefreshConversations => None,
    }
}

fn reduction_message_attention(reduction: &WorkspaceReduction) -> Option<MessageAttentionEffect> {
    reduction.effects().iter().find_map(|effect| match effect {
        WorkspaceEffect::MessageAttention(effect) => Some(effect.clone()),
        WorkspaceEffect::ConversationReadMarked { .. } => None,
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

async fn observe_huddle_socket_event(
    huddles: &HuddleActorHandle,
    team_id: Option<&str>,
    event: &SocketModeEvent,
) -> Result<()> {
    match event {
        SocketModeEvent::Message(event) => {
            let Some(room) = event.message.room.as_ref() else {
                return Ok(());
            };
            if room.has_ended() {
                let Some(call_id) = room
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|call_id| !call_id.is_empty())
                else {
                    return Ok(());
                };
                huddles
                    .observe(CoordinatorInput::HuddleEnded {
                        call_id: call_id.to_string(),
                    })
                    .await
            } else {
                let Some(huddle) =
                    team_id.and_then(|team_id| room.active_huddle(team_id, &event.channel_id))
                else {
                    return Ok(());
                };
                huddles.observe_huddle(huddle).await
            }
        }
        SocketModeEvent::UserHuddleChanged(user) => observe_huddle_user(huddles, user).await,
        SocketModeEvent::UserChanged(user)
            if user.profile.as_ref().is_some_and(|profile| {
                profile.huddle_state_call_id.is_some()
                    || profile.huddle_state_channel_id.is_some()
                    || profile.huddle_state_expiration_ts.is_some()
                    || profile.huddle_state != crate::huddles::model::SlackHuddleState::DefaultUnset
            }) =>
        {
            observe_huddle_user(huddles, user).await
        }
        SocketModeEvent::UserChanged(_)
        | SocketModeEvent::Reaction(_)
        | SocketModeEvent::RefreshConversations
        | SocketModeEvent::ConversationMarked { .. }
        | SocketModeEvent::ThreadMarked { .. } => Ok(()),
    }
}

async fn observe_huddle_messages(
    huddles: &HuddleActorHandle,
    team_id: Option<&str>,
    channel_id: &str,
    messages: &[SlackMessage],
) -> Result<()> {
    let Some(team_id) = team_id.map(str::trim).filter(|team_id| !team_id.is_empty()) else {
        return Ok(());
    };
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return Ok(());
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
        if room.has_ended() {
            let Some(call_id) = room
                .id
                .as_deref()
                .map(str::trim)
                .filter(|call_id| !call_id.is_empty())
            else {
                continue;
            };
            huddles
                .observe(CoordinatorInput::HuddleEnded {
                    call_id: call_id.to_string(),
                })
                .await?;
        } else {
            if let Some(huddle) = room.active_huddle(team_id, channel_id) {
                huddles.observe_huddle(huddle).await?;
            }
        }
    }
    Ok(())
}

async fn observe_huddle_user(huddles: &HuddleActorHandle, user: &SlackUser) -> Result<()> {
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
    huddles
        .observe(CoordinatorInput::PresenceChanged {
            user_id: user_id.to_string(),
            presence,
        })
        .await
}

async fn reject_unstarted_huddle_actor(mut receiver: HuddleActorReceiver) {
    receiver.close();
    while let Some(message) = receiver.recv().await {
        match message {
            HuddleActorMessage::Command { completion, .. } => {
                let _ = completion.send(Err(anyhow!("huddle coordinator session did not start")));
            }
            HuddleActorMessage::Observation {
                observation_slot, ..
            } => drop(observation_slot),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HuddleEffectApplication {
    Transition,
    Reset,
}

#[derive(Default)]
struct HuddleEffectRuntime {
    native_session_active: bool,
}

enum HuddleActorReceive {
    Message(HuddleActorMessage),
    Closed,
    Shutdown,
}

async fn receive_huddle_or_shutdown(
    receiver: &mut HuddleActorReceiver,
    shutdown: &mut oneshot::Receiver<()>,
) -> HuddleActorReceive {
    let mut receive = std::pin::pin!(receiver.recv());
    std::future::poll_fn(|context| {
        if std::pin::Pin::new(&mut *shutdown).poll(context).is_ready() {
            return std::task::Poll::Ready(HuddleActorReceive::Shutdown);
        }
        match receive.as_mut().poll(context) {
            std::task::Poll::Ready(Some(message)) => {
                std::task::Poll::Ready(HuddleActorReceive::Message(message))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(HuddleActorReceive::Closed),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .await
}

async fn run_huddle_actor(
    mut receiver: HuddleActorReceiver,
    events: RuntimeEventSender,
    join_capability: NativeJoinCapability,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut coordinator = HuddleCoordinator::default();
    let mut effect_runtime = HuddleEffectRuntime::default();
    match coordinator.apply(CoordinatorInput::JoinCapabilityChanged(
        join_capability.is_available(),
    )) {
        Ok(effects) => {
            if let Err(error) = apply_huddle_effects(
                &mut coordinator,
                effects,
                &events,
                join_capability,
                &mut effect_runtime,
                HuddleEffectApplication::Transition,
            ) {
                crate::debug::log(
                    "huddle",
                    &format!("HuddleCapabilityEffectsFailed error={error}"),
                );
            }
        }
        Err(error) => crate::debug::log(
            "huddle",
            &format!("HuddleCapabilityTransitionRejected error={error}"),
        ),
    }

    loop {
        match receive_huddle_or_shutdown(&mut receiver, &mut shutdown).await {
            HuddleActorReceive::Shutdown => {
                receiver.close();
                break;
            }
            HuddleActorReceive::Closed => break,
            HuddleActorReceive::Message(message) => {
                process_huddle_actor_message(
                    message,
                    &mut coordinator,
                    &events,
                    join_capability,
                    &mut effect_runtime,
                );
            }
        }
    }
    while let Some(message) = receiver.recv().await {
        process_huddle_actor_message(
            message,
            &mut coordinator,
            &events,
            join_capability,
            &mut effect_runtime,
        );
    }

    match coordinator.apply(CoordinatorInput::Reset) {
        Ok(effects) => {
            if let Err(error) = apply_huddle_effects(
                &mut coordinator,
                effects,
                &events,
                join_capability,
                &mut effect_runtime,
                HuddleEffectApplication::Reset,
            ) {
                crate::debug::log("huddle", &format!("HuddleResetEffectsFailed error={error}"));
            }
        }
        Err(error) => crate::debug::log(
            "huddle",
            &format!("HuddleResetTransitionRejected error={error}"),
        ),
    }

    let snapshot = receiver.snapshot();
    crate::debug::log(
        "huddle",
        &format!(
            "HuddleActorDrained admitted={} commands={} observations={} dequeued={} blocked={} depth={} observation_depth={} peak={} observation_peak={}",
            snapshot.admitted,
            snapshot.admitted_commands,
            snapshot.admitted_observations,
            snapshot.dequeued,
            snapshot.blocked,
            snapshot.depth,
            snapshot.observation_depth,
            snapshot.peak_depth,
            snapshot.peak_observation_depth,
        ),
    );
}

fn process_huddle_actor_message(
    message: HuddleActorMessage,
    coordinator: &mut HuddleCoordinator,
    events: &RuntimeEventSender,
    join_capability: NativeJoinCapability,
    effect_runtime: &mut HuddleEffectRuntime,
) {
    let (input, completion) = match message {
        HuddleActorMessage::Command {
            command,
            completion,
        } => (huddle_input_from_command(command), Some(completion)),
        HuddleActorMessage::Observation {
            input,
            kind,
            observation_slot,
        } => {
            debug_assert_eq!(huddle_observation_kind(&input), Some(kind));
            drop(observation_slot);
            (input, None)
        }
    };
    let result = coordinator
        .apply(input)
        .map_err(anyhow::Error::new)
        .and_then(|effects| {
            apply_huddle_effects(
                coordinator,
                effects,
                events,
                join_capability,
                effect_runtime,
                HuddleEffectApplication::Transition,
            )
        });
    if let Err(error) = result.as_ref() {
        crate::debug::log("huddle", &format!("HuddleTransitionRejected error={error}"));
    }
    if let Some(completion) = completion {
        let _ = completion.send(result);
    }
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
    effect_runtime: &mut HuddleEffectRuntime,
    application: HuddleEffectApplication,
) -> Result<()> {
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
                pending.extend(
                    coordinator
                        .apply(CoordinatorInput::Failed(failure))
                        .map_err(anyhow::Error::new)?,
                );
            }
            HuddleEffect::StopSession => {
                let had_native_session = effect_runtime.native_session_active;
                effect_runtime.native_session_active = false;
                crate::debug::log(
                    "huddle",
                    &format!(
                        "HuddleStopSessionHandled reset={} native_session_active={had_native_session}",
                        application == HuddleEffectApplication::Reset,
                    ),
                );
                if application == HuddleEffectApplication::Transition
                    && coordinator.snapshot().phase == HuddlePhase::Leaving
                {
                    pending.extend(
                        coordinator
                            .apply(CoordinatorInput::MediaStopped)
                            .map_err(anyhow::Error::new)?,
                    );
                }
            }
            HuddleEffect::OpenExternal(huddle) => events.send_event(RuntimeEventKind::Huddle(
                HuddleEvent::OpenExternalRequested(huddle),
            )),
            HuddleEffect::ApplyControls(_)
            | HuddleEffect::ApplyDeviceSelection(_)
            | HuddleEffect::StartScreenShare
            | HuddleEffect::StopScreenShare => {}
        }
    }
    Ok(())
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
    Failed,
}

impl AttentionPersistenceStatus {
    const fn metrics_outcome(self) -> AttentionPersistenceOutcome {
        match self {
            Self::NotApplicable => AttentionPersistenceOutcome::NotApplicable,
            Self::Accepted => AttentionPersistenceOutcome::Accepted,
            Self::AlreadyObserved => AttentionPersistenceOutcome::AlreadyObserved,
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
/// the complete timeline projection to the window.
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
        AttentionPersistenceStatus::AlreadyObserved => Some(DeliveryState::Duplicate),
        AttentionPersistenceStatus::Accepted
        | AttentionPersistenceStatus::NotApplicable
        | AttentionPersistenceStatus::Failed => None,
    };
    let current = workspace.apply_and_enqueue(
        Some(store),
        MutationOrigin::Realtime,
        realtime_message_mutation(&message_event, delivery),
    );
    let applied_attention = current.as_ref().and_then(reduction_message_attention);
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
        .accept_attention_delivery_for_message(channel_id, &message.ts, decision.send_notification)
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

#[cfg(test)]
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
            events.send_event(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::SyncCompleted,
            ));
            sync_client_counts(events, api, workspace.store.as_ref(), workspace.reducer).await;
            let current_huddle_channels = conversations
                .iter()
                .filter(|conversation| conversation.has_huddle_metadata())
                .map(|conversation| conversation.id.clone())
                .collect::<HashSet<_>>();
            prefetch_channel_histories_best_effort(
                events,
                api,
                workspace,
                &conversations,
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
                &conversations,
                &cached_user_names,
            )
            .await;
        }
        Err(error) => handle_conversations_load_error(events, error),
    }
    Ok(())
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
                if let Err(error) =
                    observe_huddle_messages(huddles, team_id, &channel_id, &page.messages).await
                {
                    crate::debug::log(
                        "huddle",
                        &format!("HuddlePrefetchObservationFailed error={error}"),
                    );
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
    sender: RuntimeEventMailboxSender,
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
        sender: RuntimeEventMailboxSender,
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

mod read_flusher;
pub use self::read_flusher::*;

#[cfg(test)]
mod tests;
