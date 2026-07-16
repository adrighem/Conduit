use std::collections::HashMap;
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
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::auth::{
    browser_session_token_from_env, browser_session_token_from_values, OAuthConfig,
    SlackOAuthClient, TokenStore,
};
use crate::config;
use crate::conversation_catalog::ConversationCatalog;
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation, SlackFile,
    SlackMessage, SlackUnreadState, SlackUser, SlackUserGroup, SlackUserStatus, StoredToken,
};
use crate::slack::{
    DownloadedPreviewAsset, SlackApi, SlackErrorCategory, SlackMessagePage,
    CHANNEL_HISTORY_PAGE_LIMIT,
};
use crate::socket_mode::{self, SocketModeDisconnect, SocketModeEvent, SocketModeMessageKind};
use crate::store::{StoreErrorCategory, WorkspaceStore};
use crate::thread_catalog::ThreadRecord;

const CHANNEL_HISTORY_PREFETCH_LIMIT: usize = 12;
const MAX_UNREAD_REFRESH_PASSES: usize = 3;
const UNREAD_REFRESH_RETRY_DELAY: Duration = Duration::from_secs(1);
const CONVERSATION_PATCH_BATCH_SIZE: usize = 20;
const NAVIGATION_TASK_CONCURRENCY: usize = 2;
const INTERACTIVE_TASK_CONCURRENCY: usize = 8;
const BACKGROUND_TASK_CONCURRENCY: usize = 3;
const IMAGE_TASK_CONCURRENCY: usize = 4;
const UPLOAD_TASK_CONCURRENCY: usize = 2;
const SOCKET_MODE_INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const SOCKET_MODE_MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const SOCKET_MODE_PERSISTENCE_QUEUE_CAPACITY: usize = 128;
const ATTACHMENT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const ATTACHMENT_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const ATTACHMENT_BASENAME_MAX_BYTES: usize = 180;

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
    UploadFile {
        channel_id: String,
        thread_ts: Option<String>,
        path: PathBuf,
        initial_comment: Option<String>,
        remove_after_upload: bool,
    },
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
    PostMessage,
    Reaction,
    Saved,
    FileUpload,
    SocketMode,
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
    Image(String),
    Media(String),
    Attachment(String),
    Message {
        channel_id: String,
        thread_ts: Option<String>,
    },
    Upload {
        channel_id: String,
        thread_ts: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OperationContext {
    pub operation: RuntimeOperation,
    pub target: RuntimeTarget,
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

#[derive(Debug)]
pub enum RuntimeEventKind {
    Status(String),
    Error(RuntimeFailure),
    RuntimeStartFailed(RuntimeFailure),
    SignedOut,
    Authenticated(AuthInfo),
    ConversationsLoaded(Vec<SlackConversation>),
    ConversationsLoadFailed(RuntimeFailure),
    ConversationChannelsDiscovered(Vec<SlackConversation>),
    ConversationPeopleDiscovered(Vec<SlackUser>),
    ConversationOpened(SlackConversation),
    ConversationLeft {
        channel_id: String,
    },
    ConversationsPatched {
        conversations: Vec<SlackConversation>,
        unread_states: Vec<(String, SlackUnreadState, Option<String>)>,
    },
    ConversationUnreadUpdated {
        channel_id: String,
        unread_state: SlackUnreadState,
    },
    ConversationMarkedRead {
        channel_id: String,
        ts: String,
    },
    ConversationNotificationCandidate {
        channel_id: String,
        messages: Vec<SlackMessage>,
    },
    ThreadCatalogLoaded(Vec<ThreadRecord>),
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
    SavedItemsLoaded(Vec<SavedItem>),
    UserLoaded {
        user_id: String,
        display_name: String,
        status: Option<SlackUserStatus>,
    },
    UserProfileLoaded(SlackUser),
    UserNamesLoaded(HashMap<String, String>),
    UserSearchAliasesLoaded(HashMap<String, Vec<String>>),
    UserStatusesLoaded(HashMap<String, SlackUserStatus>),
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
    MessagePosted {
        channel_id: String,
        message: Box<SlackMessage>,
    },
    ReactionUpdated {
        channel_id: String,
        thread_ts: Option<String>,
    },
    SavedUpdated {
        channel_id: String,
        saved: bool,
        thread_ts: Option<String>,
    },
    SocketModeEvent(SocketModeEvent),
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
            Self::ConversationsLoaded(_)
            | Self::ConversationsPatched { .. }
            | Self::ConversationsLoadFailed(_)
            | Self::ConversationUnreadUpdated { .. }
            | Self::ThreadCatalogLoaded(_)
            | Self::ConversationNotificationCandidate { .. } => {
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
            }
            Self::ConversationMarkedRead { channel_id, .. } => OperationContext::new(
                RuntimeOperation::ReadMarker,
                RuntimeTarget::Channel(channel_id.clone()),
            ),
            Self::ConversationChannelsDiscovered(_) | Self::ConversationPeopleDiscovered(_) => {
                OperationContext::new(
                    RuntimeOperation::ConversationDiscovery,
                    RuntimeTarget::Workspace,
                )
            }
            Self::ConversationOpened(conversation) => OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel(conversation.id.clone()),
            ),
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
            | Self::UserSearchAliasesLoaded(_)
            | Self::UserStatusesLoaded(_)
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
            Self::SocketModeEvent(_) => {
                OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace)
            }
            Self::RuntimeStartFailed(_) => {
                OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace)
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
    workspace_store: Option<WorkspaceStore>,
    user_cache: Arc<Mutex<HashMap<String, String>>>,
    read_marks: Arc<Mutex<HashMap<String, String>>>,
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

struct RuntimeState {
    active_session: SessionId,
    connection: Option<RuntimeConnection>,
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
            tasks: HashMap::new(),
            task_requests: HashMap::new(),
            active_requests: HashMap::new(),
            latest_requests: HashMap::new(),
            active_navigation: HashMap::new(),
            latest_navigation: HashMap::new(),
            next_task_id: 0,
        }
    }

    fn replace_session(&mut self, session: SessionId) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        self.active_requests.clear();
        self.latest_requests.clear();
        self.task_requests.clear();
        self.active_navigation.clear();
        self.latest_navigation.clear();
        self.active_session = session;
        self.connection = None;
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
    let task = tokio::spawn(async move {
        if task_started.await.is_err() {
            return;
        }
        future.await;
        state_after_task
            .lock()
            .expect("runtime state lock poisoned")
            .finish_task(task_id, request_after_task.as_ref());
    });
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
                    let _ = events.send(RuntimeEvent {
                        meta: RuntimeEventMeta::new(
                            RuntimeIdentity {
                                session: SessionId::default().next(),
                                request: RequestId::new(1),
                            },
                            context,
                        ),
                        kind,
                    });
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
        {
            let mut runtime_state = state.lock().expect("runtime state lock poisoned");
            if identity.session < runtime_state.active_session {
                continue;
            }
            if identity.session > runtime_state.active_session {
                runtime_state.replace_session(identity.session);
            }
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

    state
        .lock()
        .expect("runtime state lock poisoned")
        .replace_session(SessionId::default());
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
                        events.send_failure(&error);
                        return;
                    }
                },
                Err(error) => {
                    events.send_failure(&error);
                    return;
                }
            };

            let Some(token) = token else {
                events.send_event(RuntimeEventKind::SignedOut);
                return;
            };
            let oauth = oauth.clone();
            spawn_authentication_task(state, identity, events, limits.clone(), async move {
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
            events.send_status("Opening Slack authorization");
            let oauth = oauth.clone();
            spawn_authentication_task(state, identity, events, limits.clone(), async move {
                let token = oauth
                    .authenticate(OAuthConfig::new(client_id), debug_auth)
                    .await?;
                authenticate_token(token).await
            });
        }
        RuntimeCommand::StartBrowserSession {
            xoxc_token,
            xoxd_token,
            user_agent,
        } => {
            events.send_status("Validating Slack browser session");
            let token = match browser_session_token_from_values(
                Some(xoxc_token),
                Some(xoxd_token),
                user_agent,
            ) {
                Ok(Some(token)) => token,
                Ok(None) => {
                    events.send_event(RuntimeEventKind::Error(RuntimeFailure::validation(
                        "Enter XOXC and XOXD tokens",
                    )));
                    return;
                }
                Err(error) => {
                    events.send_failure(&error);
                    return;
                }
            };
            spawn_authentication_task(
                state,
                identity,
                events,
                limits.clone(),
                authenticate_token(token),
            );
        }
        RuntimeCommand::SignOut => {
            finish_sign_out(&events, TokenStore.clear());
        }
        RuntimeCommand::Disconnect => {}
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
                    let connection = {
                        let mut runtime_state =
                            state_for_task.lock().expect("runtime state lock poisoned");
                        if runtime_state.active_session != identity.session {
                            return;
                        }
                        if let Err(error) = TokenStore.save(&token) {
                            events.send_failure(&error);
                            return;
                        }
                        let connection = RuntimeConnection {
                            slack: api,
                            workspace_store: Some(WorkspaceStore::new(
                                config::state_cache_dir(),
                                &workspace_store_id(&auth),
                            )),
                            user_cache: Arc::new(Mutex::new(HashMap::new())),
                            read_marks: Arc::new(Mutex::new(HashMap::new())),
                        };
                        runtime_state.connection = Some(connection.clone());
                        connection
                    };

                    let current_user_id = auth.user_id.clone();
                    events.send_event(RuntimeEventKind::Authenticated(auth));
                    spawn_workspace_tasks(
                        &state_for_task,
                        identity,
                        events,
                        connection,
                        limits,
                        current_user_id,
                    );
                }
                Err(error) => events.send_failure(&error),
            }
        },
    );
}

fn spawn_workspace_tasks(
    state: &Arc<Mutex<RuntimeState>>,
    identity: RuntimeIdentity,
    events: RuntimeEventSender,
    connection: RuntimeConnection,
    limits: RuntimeTaskLimits,
    current_user_id: Option<String>,
) {
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
        load_cached_user_names_shared(&hydration_events, &hydration_connection).await;
        load_cached_user_search_aliases(&hydration_events, &hydration_connection.workspace_store)
            .await;
        load_cached_user_statuses(&hydration_events, &hydration_connection.workspace_store).await;
        load_cached_conversations(&hydration_events, &hydration_connection.workspace_store).await;
        load_cached_thread_catalog(&hydration_events, &hydration_connection.workspace_store).await;
        load_cached_custom_emojis(&hydration_events, &hydration_connection.workspace_store).await;

        let emoji_events = hydration_events.clone();
        let emoji_connection = hydration_connection.clone();
        let emoji_limits = hydration_limits.clone();
        spawn_request_task(
            &state_after_hydration,
            TrackedRequest::new(
                identity,
                OperationContext::new(RuntimeOperation::Emoji, RuntimeTarget::Workspace),
            ),
            async move {
                let _permit = emoji_limits.acquire(RuntimeTaskLane::Background).await;
                match emoji_connection.slack.custom_emojis().await {
                    Ok(emojis) => {
                        if let Some(store) = emoji_connection.workspace_store.as_ref() {
                            if let Err(error) = store.store_custom_emojis(&emojis).await {
                                crate::debug::log(
                                    "store",
                                    &format!("CustomEmojiStoreFailed error={error:#}"),
                                );
                            }
                        }
                        emoji_events.send_event(RuntimeEventKind::EmojiCatalogLoaded(emojis));
                    }
                    Err(error) => crate::debug::log(
                        "runtime",
                        &format!("CustomEmojiRefreshFailed error={error:#}"),
                    ),
                }
            },
        );

        let refresh_events = hydration_events.clone();
        let refresh_connection = hydration_connection.clone();
        let refresh_limits = hydration_limits.clone();
        spawn_request_task(
            &state_after_hydration,
            TrackedRequest::new(
                identity,
                OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
            ),
            async move {
                let _permit = refresh_limits.acquire(RuntimeTaskLane::Background).await;
                let cached_user_names = refresh_connection
                    .user_cache
                    .lock()
                    .expect("runtime user cache lock poisoned")
                    .clone();
                if let Err(error) = load_conversations_best_effort_with_api(
                    &refresh_events,
                    &refresh_connection.slack,
                    &refresh_connection.workspace_store,
                    cached_user_names,
                )
                .await
                {
                    crate::debug::log(
                        "runtime",
                        &format!("ConversationsBackgroundRefreshFailed error={error:#}"),
                    );
                }
            },
        );

        let group_events = hydration_events;
        let group_connection = hydration_connection;
        let group_limits = hydration_limits;
        spawn_request_task(
            &state_after_hydration,
            TrackedRequest::new(
                identity,
                OperationContext::new(RuntimeOperation::User, RuntimeTarget::Workspace),
            ),
            async move {
                let _permit = group_limits.acquire(RuntimeTaskLane::Background).await;
                let cached_user_names = group_connection
                    .user_cache
                    .lock()
                    .expect("runtime user cache lock poisoned")
                    .clone();
                load_user_groups_best_effort_with_api(
                    &group_events,
                    &group_connection.slack,
                    &group_connection.workspace_store,
                    cached_user_names,
                )
                .await;
            },
        );
    });

    if let Some(app_token) = config::slack_app_token() {
        let socket_events = events.unsolicited(OperationContext::new(
            RuntimeOperation::SocketMode,
            RuntimeTarget::Workspace,
        ));
        spawn_session_task(
            state,
            identity.session,
            run_socket_mode(
                app_token,
                socket_events,
                connection.workspace_store.clone(),
                current_user_id,
            ),
        );
    }
}

async fn load_cached_user_names_shared(
    events: &RuntimeEventSender,
    connection: &RuntimeConnection,
) {
    let mut cached_names = HashMap::new();
    load_cached_user_names(events, &connection.workspace_store, &mut cached_names).await;
    connection
        .user_cache
        .lock()
        .expect("runtime user cache lock poisoned")
        .extend(cached_names);
}

async fn load_cached_user_search_aliases(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    match store.load_user_search_aliases().await {
        Ok(aliases) if !aliases.is_empty() => {
            events.send_event(RuntimeEventKind::UserSearchAliasesLoaded(aliases));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedUserSearchAliasesLoadFailed error={error:#}"),
        ),
    }
}

async fn load_cached_user_statuses(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    match store.load_user_statuses().await {
        Ok(statuses) if !statuses.is_empty() => {
            events.send_event(RuntimeEventKind::UserStatusesLoaded(statuses));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedUserStatusesLoadFailed error={error:#}"),
        ),
    }
}

fn user_statuses(users: &[SlackUser]) -> HashMap<String, SlackUserStatus> {
    users
        .iter()
        .filter_map(|user| Some((user.id.clone()?, user.status()?)))
        .collect()
}

async fn load_cached_custom_emojis(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    match store.load_custom_emojis().await {
        Ok(emojis) if !emojis.is_empty() => {
            events.send_event(RuntimeEventKind::EmojiCatalogLoaded(emojis));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "store",
            &format!("CustomEmojiCacheLoadFailed error={error:#}"),
        ),
    }
}

async fn handle_connected_command(
    command: RuntimeCommand,
    connection: RuntimeConnection,
    events: &RuntimeEventSender,
    image_cache: &ImageAssetCache,
) -> Result<()> {
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
        user_cache: &mut user_cache,
        read_marks: &mut read_marks,
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
    user_cache: &'a mut HashMap<String, String>,
    read_marks: &'a mut HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationRefreshMode {
    Background,
}

fn conversation_refresh_mode() -> ConversationRefreshMode {
    ConversationRefreshMode::Background
}

fn cached_dm_user_ids(
    conversations: &[SlackConversation],
    user_cache: &HashMap<String, String>,
) -> Vec<String> {
    let mut user_ids = conversations
        .iter()
        .filter(|conversation| conversation.is_im.unwrap_or(false))
        .filter_map(|conversation| conversation.user.as_deref())
        .filter(|user_id| user_cache.contains_key(*user_id))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    user_ids.sort();
    user_ids.dedup();
    user_ids
}

fn recent_history_preview(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages.truncate(CHANNEL_HISTORY_PAGE_LIMIT);
    messages
}

#[derive(Debug, Clone)]
struct ChannelHistoryPrefetchCandidate {
    id: String,
    unread: bool,
    direct_message: bool,
    unread_count: u64,
    activity_score: f64,
    title: String,
}

fn channel_history_prefetch_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter_map(channel_history_prefetch_candidate)
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .unread
            .cmp(&left.unread)
            .then_with(|| right.unread_count.cmp(&left.unread_count))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    let (urgent_direct_messages, mut remaining): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|candidate| candidate.unread && candidate.direct_message);
    remaining.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT);
    urgent_direct_messages
        .into_iter()
        .chain(remaining)
        .map(|candidate| candidate.id)
        .collect()
}

fn conversation_unread_refresh_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| !conversation.id.trim().is_empty())
        .map(|conversation| ChannelHistoryPrefetchCandidate {
            id: conversation.id.clone(),
            unread: conversation.has_unread_activity(),
            direct_message: conversation.is_im.unwrap_or(false)
                || conversation.is_mpim.unwrap_or(false),
            unread_count: conversation.unread_activity_count(),
            activity_score: conversation_activity_score(conversation),
            title: conversation.display_name().to_lowercase(),
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .unread
            .cmp(&left.unread)
            .then_with(|| right.unread_count.cmp(&left.unread_count))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.dedup_by(|left, right| left.id == right.id);
    candidates
        .into_iter()
        .map(|candidate| candidate.id)
        .collect()
}

fn channel_history_prefetch_candidate(
    conversation: &SlackConversation,
) -> Option<ChannelHistoryPrefetchCandidate> {
    if conversation.is_archived.unwrap_or(false) {
        return None;
    }

    let is_channel = conversation.is_channel.unwrap_or(false)
        || conversation.is_group.unwrap_or(false)
        || conversation.is_private.unwrap_or(false)
        || conversation.is_im.unwrap_or(false)
        || conversation.is_mpim.unwrap_or(false);
    if !is_channel
        || ((conversation.is_im.unwrap_or(false) || conversation.is_mpim.unwrap_or(false))
            && !conversation.has_unread_activity())
    {
        return None;
    }

    Some(ChannelHistoryPrefetchCandidate {
        id: conversation.id.clone(),
        unread: conversation.has_unread_activity(),
        direct_message: conversation.is_im.unwrap_or(false)
            || conversation.is_mpim.unwrap_or(false),
        unread_count: conversation.unread_activity_count(),
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
        | RuntimeCommand::Disconnect => {
            return Err(anyhow!("session command reached connected task handler"));
        }
        RuntimeCommand::RefreshConversations => {
            crate::debug::log("runtime", "RefreshConversations");
            debug_assert_eq!(
                conversation_refresh_mode(),
                ConversationRefreshMode::Background
            );
            let api = require_slack(context.slack)?.clone();
            let workspace_store = (*context.workspace_store).clone();
            let cached_user_names = context.user_cache.clone();
            load_conversations_best_effort_with_api(
                context.events,
                &api,
                &workspace_store,
                cached_user_names,
            )
            .await?;
        }
        RuntimeCommand::DiscoverConversations => {
            let api = require_slack(context.slack)?;
            let channels = api.discover_conversations().await?;
            context
                .events
                .send_event(RuntimeEventKind::ConversationChannelsDiscovered(channels));
            let users = api.users().await?;
            let aliases = users
                .iter()
                .filter_map(|user| Some((user.id.clone()?, user.search_aliases())))
                .collect::<HashMap<_, _>>();
            let statuses = user_statuses(&users);
            if let Some(store) = context.workspace_store.as_ref() {
                store.store_user_search_aliases(&aliases).await?;
                store.store_user_statuses(&statuses).await?;
            }
            context
                .events
                .send_event(RuntimeEventKind::UserSearchAliasesLoaded(aliases));
            context
                .events
                .send_event(RuntimeEventKind::UserStatusesLoaded(statuses));
            context
                .events
                .send_event(RuntimeEventKind::ConversationPeopleDiscovered(users));
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
            if let Some(store) = context.workspace_store.as_ref() {
                store.store_conversation(&conversation).await?;
            }
            context
                .events
                .send_event(RuntimeEventKind::ConversationOpened(conversation));
        }
        RuntimeCommand::LeaveConversation { channel_id } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Leaving channel");
            api.leave_conversation(&channel_id).await?;
            if let Some(store) = context.workspace_store.as_ref() {
                store.remove_conversation(&channel_id).await?;
            }
            context
                .events
                .send_event(RuntimeEventKind::ConversationLeft { channel_id });
        }
        RuntimeCommand::OpenDirectMessage { user_id } => {
            let api = require_slack(context.slack)?;
            context.events.send_status("Opening direct message");
            let mut conversation = api.open_direct_message(&user_id).await?;
            conversation.user = Some(user_id);
            conversation.is_im = Some(true);
            if let Some(store) = context.workspace_store.as_ref() {
                store.store_conversation(&conversation).await?;
            }
            context
                .events
                .send_event(RuntimeEventKind::ConversationOpened(conversation));
        }
        RuntimeCommand::LoadHistory { channel_id } => {
            let api = require_slack(context.slack)?;
            crate::debug::log("runtime", &format!("LoadHistory channel_id={channel_id}"));
            load_cached_history(context.events, context.workspace_store, &channel_id).await;
            context.events.send_status("Loading conversation");
            let page = api.history(&channel_id).await?;
            observe_thread_history(
                context.events,
                context.workspace_store,
                &channel_id,
                &page.messages,
            )
            .await;
            store_history(context.workspace_store, &channel_id, &page.messages).await;
            crate::debug::log(
                "runtime",
                &format!(
                    "HistoryLoaded channel_id={channel_id} messages={} has_more={} next_cursor={}",
                    page.messages.len(),
                    page.has_more,
                    page.next_cursor.is_some()
                ),
            );
            send_history_loaded(context.events, channel_id, page, false);
        }
        RuntimeCommand::LoadOlderHistory { channel_id, cursor } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadOlderHistory channel_id={channel_id}"),
            );
            context.events.send_status("Loading older messages");
            let page = api.history_page(&channel_id, Some(&cursor)).await?;
            observe_thread_history(
                context.events,
                context.workspace_store,
                &channel_id,
                &page.messages,
            )
            .await;
            store_merged_history(context.workspace_store, &channel_id, &page.messages).await;
            send_history_loaded(context.events, channel_id, page, true);
        }
        RuntimeCommand::LoadThread { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            load_cached_thread(context.events, context.workspace_store, &channel_id, &ts).await;
            context.events.send_status("Loading thread");
            let page = api.thread_replies(&channel_id, &ts).await?;
            observe_thread_page(
                context.events,
                context.workspace_store,
                &channel_id,
                &ts,
                &page.messages,
                !page.has_more && page.next_cursor.is_none(),
            )
            .await;
            store_thread(context.workspace_store, &channel_id, &ts, &page.messages).await;
            send_thread_loaded(context.events, channel_id, ts, page, false);
        }
        RuntimeCommand::LoadOlderThread {
            channel_id,
            ts,
            cursor,
        } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadOlderThread channel_id={channel_id} ts={ts}"),
            );
            context.events.send_status("Loading more replies");
            let page = api
                .thread_replies_page(&channel_id, &ts, Some(&cursor))
                .await?;
            if let Some(store) = context.workspace_store.as_ref() {
                if let Err(error) = store
                    .store_merged_thread(&channel_id, &ts, &page.messages)
                    .await
                {
                    crate::debug::log(
                        "store",
                        &format!("ThreadMergeStoreFailed channel_id={channel_id} ts={ts} error={error:#}"),
                    );
                }
            }
            observe_thread_page(
                context.events,
                context.workspace_store,
                &channel_id,
                &ts,
                &page.messages,
                !page.has_more && page.next_cursor.is_none(),
            )
            .await;
            send_thread_loaded(context.events, channel_id, ts, page, true);
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
                    status: None,
                });
            } else {
                let api = require_slack(context.slack)?;
                let user = api.user(&user_id).await?;
                let display_name = user.display_name().unwrap_or_else(|| user_id.clone());
                let status = user.status();
                context
                    .user_cache
                    .insert(user_id.clone(), display_name.clone());
                store_user_name(context.workspace_store, &user_id, &display_name).await;
                if let Some(store) = context.workspace_store.as_ref() {
                    if let Err(error) = store.store_user_status(&user_id, status.clone()).await {
                        crate::debug::log(
                            "store",
                            &format!(
                                "CachedUserStatusStoreFailed user_id={user_id} error={error:#}"
                            ),
                        );
                    }
                }
                context.events.send_event(RuntimeEventKind::UserLoaded {
                    user_id,
                    display_name,
                    status,
                });
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
                .send_event(RuntimeEventKind::UserProfileLoaded(user));
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
        RuntimeCommand::MarkConversationRead { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            mark_conversation_read_best_effort(
                api,
                context.events,
                context.read_marks,
                context.workspace_store,
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
            if let Some(store) = context.workspace_store.as_ref() {
                store.mark_thread_read(&channel_id, &thread_ts, &ts).await?;
                load_cached_thread_catalog(context.events, context.workspace_store).await;
            }
        }
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let message = api
                .post_message(&channel_id, &text, thread_ts.as_deref())
                .await?;
            context.events.send_event(RuntimeEventKind::MessagePosted {
                channel_id,
                message: Box::new(message),
            });
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
            context
                .events
                .send_event(RuntimeEventKind::ReactionUpdated {
                    channel_id,
                    thread_ts,
                });
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

async fn run_socket_mode(
    app_token: String,
    events: RuntimeEventSender,
    workspace_store: Option<WorkspaceStore>,
    current_user_id: Option<String>,
) {
    let mut reconnect_delay = SOCKET_MODE_INITIAL_RECONNECT_DELAY;

    loop {
        let events_for_run = events.clone();
        let mut persistence_tasks = tokio::task::JoinSet::new();
        let persistence_sender = workspace_store.clone().map(|store| {
            let (sender, receiver) = mpsc::channel(SOCKET_MODE_PERSISTENCE_QUEUE_CAPACITY);
            persistence_tasks.spawn(persist_realtime_events(
                receiver,
                store,
                current_user_id.clone(),
                events_for_run.clone(),
            ));
            sender
        });
        let persistence_for_run = persistence_sender.clone();
        let result = socket_mode::run_once(&app_token, move |event| {
            if let Some(sender) = persistence_for_run.as_ref() {
                let persistence_event = match &event {
                    SocketModeEvent::UserChanged(user) => {
                        Some(RealtimePersistenceEvent::UserChanged(user.clone()))
                    }
                    SocketModeEvent::Message(message) => {
                        Some(RealtimePersistenceEvent::Message(message.clone()))
                    }
                    SocketModeEvent::Reaction(_) | SocketModeEvent::RefreshConversations => None,
                };
                if let Some(persistence_event) = persistence_event {
                    if let Err(error) = sender.try_send(persistence_event) {
                        crate::debug::log(
                            "store",
                            &format!("RealtimePersistenceQueueRejected error={error}"),
                        );
                    }
                }
            }
            events_for_run.send_event(RuntimeEventKind::SocketModeEvent(event));
        })
        .await;
        drop(persistence_sender);
        while let Some(join_result) = persistence_tasks.join_next().await {
            if let Err(error) = join_result {
                crate::debug::log(
                    "store",
                    &format!("RealtimePersistenceWorkerFailed error={error}"),
                );
            }
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
        tokio::time::sleep(timing.sleep).await;
    }
}

#[derive(Debug)]
enum RealtimePersistenceEvent {
    UserChanged(SlackUser),
    Message(Box<crate::socket_mode::SocketModeMessageEvent>),
}

async fn persist_realtime_events(
    mut receiver: mpsc::Receiver<RealtimePersistenceEvent>,
    store: WorkspaceStore,
    current_user_id: Option<String>,
    events: RuntimeEventSender,
) {
    while let Some(event) = receiver.recv().await {
        match event {
            RealtimePersistenceEvent::UserChanged(user) => {
                let Some(user_id) = user.id.as_deref() else {
                    continue;
                };
                if let Err(error) = store.store_user_status(user_id, user.status()).await {
                    crate::debug::log(
                        "store",
                        &format!("RealtimeUserStatusStoreFailed user_id={user_id} error={error:#}"),
                    );
                }
            }
            RealtimePersistenceEvent::Message(message) => {
                persist_realtime_message(&store, &current_user_id, &events, *message).await;
            }
        }
    }
}

async fn persist_realtime_message(
    store: &WorkspaceStore,
    current_user_id: &Option<String>,
    events: &RuntimeEventSender,
    message_event: crate::socket_mode::SocketModeMessageEvent,
) {
    if message_event.kind != SocketModeMessageKind::Posted {
        return;
    }
    let channel_id = message_event.channel_id;
    let message = message_event.message;
    if message.user.as_deref() != current_user_id.as_deref() {
        if let Err(error) = store
            .mark_conversation_unread_from_event(&channel_id, &message.ts)
            .await
        {
            crate::debug::log(
                "store",
                &format!("ConversationRealtimeStoreFailed channel_id={channel_id} error={error:#}"),
            );
        }
    }
    if let Err(error) = store
        .store_merged_history(&channel_id, std::slice::from_ref(&message))
        .await
    {
        crate::debug::log(
            "store",
            &format!(
                "ConversationRealtimeHistoryStoreFailed channel_id={channel_id} error={error:#}"
            ),
        );
    }
    if let Err(error) = store
        .observe_thread_realtime(&channel_id, &message, current_user_id.as_deref())
        .await
    {
        crate::debug::log(
            "store",
            &format!("ThreadRealtimeStoreFailed channel_id={channel_id} error={error:#}"),
        );
    } else {
        match store.load_thread_catalog().await {
            Ok(records) if !records.is_empty() => {
                events.send_event(RuntimeEventKind::ThreadCatalogLoaded(records));
            }
            Ok(_) => {}
            Err(error) => {
                crate::debug::log("store", &format!("ThreadCatalogLoadFailed error={error:#}"))
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

async fn load_user_groups_best_effort_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    cached_user_names: HashMap<String, String>,
) {
    let groups = match api.user_groups().await {
        Ok(groups) => groups,
        Err(error) => {
            crate::debug::log("runtime", &format!("UserGroupsLoadFailed error={error:#}"));
            return;
        }
    };

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

async fn load_conversations_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
) -> Result<Vec<SlackConversation>> {
    events.send_status("Loading conversations");
    let fresh = api.conversations().await?;
    let conversations = if let Some(store) = workspace_store.as_ref() {
        store.reconcile_conversations(fresh).await?
    } else {
        reconcile_conversation_snapshot(Vec::new(), fresh)?
    };
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoaded count={}", conversations.len()),
    );
    events.send_event(RuntimeEventKind::ConversationsLoaded(conversations.clone()));
    Ok(conversations)
}

fn reconcile_conversation_snapshot(
    cached: Vec<SlackConversation>,
    fresh: Vec<SlackConversation>,
) -> Result<Vec<SlackConversation>> {
    if fresh.is_empty() && !cached.is_empty() {
        return Err(anyhow!(
            "Slack returned an unexpectedly empty conversation membership snapshot"
        ));
    }

    let mut catalog = ConversationCatalog::from_cached(cached);
    let mut snapshot = catalog.begin_membership_snapshot();
    for conversation in fresh {
        snapshot.upsert(conversation);
    }
    catalog.commit_membership_snapshot(snapshot);
    Ok(catalog.conversations())
}

async fn load_conversations_best_effort_with_api(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    cached_user_names: HashMap<String, String>,
) -> Result<()> {
    match load_conversations_with_api(events, api, workspace_store).await {
        Ok(conversations) => {
            let unread_refresh_candidates = conversation_unread_refresh_candidates(&conversations);
            refresh_conversation_unread_states_best_effort(
                events,
                api,
                workspace_store,
                unread_refresh_candidates.iter(),
            )
            .await;
            let refreshed_conversations = if let Some(store) = workspace_store.as_ref() {
                match store.load_conversations().await {
                    Ok(Some(refreshed)) => refreshed,
                    Ok(None) => conversations.clone(),
                    Err(error) => {
                        crate::debug::log(
                            "store",
                            &format!("ConversationPrefetchCatalogLoadFailed error={error:#}"),
                        );
                        conversations.clone()
                    }
                }
            } else {
                conversations.clone()
            };
            prefetch_channel_histories_best_effort(
                events,
                api,
                workspace_store,
                &refreshed_conversations,
            )
            .await;
            refresh_cached_dm_user_names(
                events,
                api,
                workspace_store,
                &conversations,
                &cached_user_names,
            )
            .await;
        }
        Err(error) => handle_conversations_load_error(events, error),
    }
    Ok(())
}

async fn refresh_conversation_unread_states_best_effort<'a>(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    channel_ids: impl IntoIterator<Item = &'a String>,
) {
    let mut pending = channel_ids.into_iter().cloned().collect::<Vec<_>>();
    if let Some(store) = workspace_store.as_ref() {
        match store.load_pending_unread_refresh().await {
            Ok(cached_pending) => pending.extend(cached_pending),
            Err(error) => crate::debug::log(
                "store",
                &format!("PendingUnreadRefreshLoadFailed error={error:#}"),
            ),
        }
    }
    pending.sort();
    pending.dedup();
    if let Some(store) = workspace_store.as_ref() {
        if let Err(error) = store.store_pending_unread_refresh(&pending).await {
            crate::debug::log(
                "store",
                &format!("PendingUnreadRefreshStoreFailed error={error:#}"),
            );
        }
    }
    let mut enriched_batch = Vec::new();
    let mut unread_batch = Vec::new();
    for pass in 0..MAX_UNREAD_REFRESH_PASSES {
        let mut failed = Vec::new();
        for channel_id in std::mem::take(&mut pending) {
            match api.conversation_with_unread_state(&channel_id).await {
                Ok((details, unread_state)) => {
                    let server_last_read = details.as_ref().and_then(|details| {
                        details.extra.get("last_read")?.as_str().map(str::to_string)
                    });
                    if let Some(mut details) = details {
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
                        if let Some(store) = workspace_store.as_ref() {
                            if let Err(error) = store.merge_conversation(&details).await {
                                crate::debug::log(
                                    "store",
                                    &format!("ConversationEnrichmentStoreFailed channel_id={channel_id} error={error:#}"),
                                );
                            }
                        }
                        enriched_batch.push(details);
                    }
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ConversationUnreadRefreshed channel_id={channel_id} known={} unread={} display_count={}",
                            unread_state.known, unread_state.has_unread, unread_state.display_count
                        ),
                    );
                    if unread_state.known
                        && store_conversation_unread_state(
                            workspace_store,
                            &channel_id,
                            unread_state,
                            server_last_read.as_deref(),
                        )
                        .await
                    {
                        unread_batch.push((channel_id.clone(), unread_state, server_last_read));
                    } else if !unread_state.known {
                        failed.push(channel_id.clone());
                    }
                    if enriched_batch.len() + unread_batch.len() >= CONVERSATION_PATCH_BATCH_SIZE {
                        send_conversation_patch_batch(
                            events,
                            &mut enriched_batch,
                            &mut unread_batch,
                        );
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
        if let Err(error) = store.store_pending_unread_refresh(&pending).await {
            crate::debug::log(
                "store",
                &format!("PendingUnreadRefreshStoreFailed error={error:#}"),
            );
        }
    }
    send_conversation_patch_batch(events, &mut enriched_batch, &mut unread_batch);
}

fn send_conversation_patch_batch(
    events: &RuntimeEventSender,
    conversations: &mut Vec<SlackConversation>,
    unread_states: &mut Vec<(String, SlackUnreadState, Option<String>)>,
) {
    if conversations.is_empty() && unread_states.is_empty() {
        return;
    }
    events.send_event(RuntimeEventKind::ConversationsPatched {
        conversations: std::mem::take(conversations),
        unread_states: std::mem::take(unread_states),
    });
}

async fn prefetch_channel_histories_best_effort(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    let channel_ids = channel_history_prefetch_candidates(conversations);
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

        match api.history(&channel_id).await {
            Ok(page) => {
                if store_conversation_unread_state(
                    workspace_store,
                    &channel_id,
                    page.unread_state,
                    None,
                )
                .await
                {
                    send_conversation_unread_update(events, &channel_id, page.unread_state);
                }
                send_conversation_notification_candidate(events, &channel_id, &page.messages);
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ChannelHistoryPrefetched channel_id={channel_id} messages={}",
                        page.messages.len()
                    ),
                );
                store_history(workspace_store, &channel_id, &page.messages).await;
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("ChannelHistoryPrefetchFailed channel_id={channel_id} error={error:#}"),
            ),
        }
    }
}

fn send_conversation_notification_candidate(
    events: &RuntimeEventSender,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    if !messages.is_empty() {
        events.send_event(RuntimeEventKind::ConversationNotificationCandidate {
            channel_id: channel_id.to_string(),
            messages: messages.to_vec(),
        });
    }
}

fn send_conversation_unread_update(
    events: &RuntimeEventSender,
    channel_id: &str,
    unread_state: SlackUnreadState,
) {
    if unread_state.known {
        events.send_event(RuntimeEventKind::ConversationUnreadUpdated {
            channel_id: channel_id.to_string(),
            unread_state,
        });
    }
}

async fn load_cached_user_names(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    user_cache: &mut HashMap<String, String>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_user_names().await {
        Ok(user_names) if !user_names.is_empty() => {
            crate::debug::log(
                "runtime",
                &format!("CachedUserNamesLoaded count={}", user_names.len()),
            );
            user_cache.extend(user_names.clone());
            events.send_event(RuntimeEventKind::UserNamesLoaded(user_names));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedUserNamesLoadFailed error={error:#}"),
        ),
    }
}

async fn refresh_cached_dm_user_names(
    events: &RuntimeEventSender,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
    cached_user_names: &HashMap<String, String>,
) {
    let user_ids = cached_dm_user_ids(conversations, cached_user_names);
    if user_ids.is_empty() {
        return;
    }

    let mut refreshed = HashMap::new();
    for user_id in user_ids {
        match api.user_display_name(&user_id).await {
            Ok(display_name) => {
                refreshed.insert(user_id, display_name);
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("UserDisplayNameRefreshFailed user_id={user_id} error={error:#}"),
            ),
        }
    }

    if refreshed.is_empty() {
        return;
    }

    store_user_names(workspace_store, &refreshed).await;
    events.send_event(RuntimeEventKind::UserNamesLoaded(refreshed));
}

fn handle_conversations_load_error(events: &RuntimeEventSender, error: anyhow::Error) {
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoadFailed error={error:#}"),
    );
    events.send_event(RuntimeEventKind::ConversationsLoadFailed(
        RuntimeFailure::from_error(&error),
    ));
}

fn send_history_loaded(
    events: &RuntimeEventSender,
    channel_id: String,
    page: SlackMessagePage,
    append_older: bool,
) {
    events.send_event(RuntimeEventKind::HistoryLoaded {
        channel_id,
        messages: page.messages,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        append_older,
        cached: false,
    });
}

fn send_thread_loaded(
    events: &RuntimeEventSender,
    channel_id: String,
    ts: String,
    page: SlackMessagePage,
    append_older: bool,
) {
    events.send_event(RuntimeEventKind::ThreadLoaded {
        channel_id,
        ts,
        messages: page.messages,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        append_older,
    });
}

async fn mark_conversation_read_best_effort(
    api: &SlackApi,
    events: &RuntimeEventSender,
    read_marks: &mut HashMap<String, String>,
    workspace_store: &Option<WorkspaceStore>,
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
        clear_cached_conversation_unread(workspace_store, channel_id, latest_ts).await;
        events.send_event(RuntimeEventKind::ConversationMarkedRead {
            channel_id: channel_id.to_string(),
            ts: latest_ts.to_string(),
        });
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
    clear_cached_conversation_unread(workspace_store, channel_id, latest_ts).await;
    events.send_event(RuntimeEventKind::ConversationMarkedRead {
        channel_id: channel_id.to_string(),
        ts: latest_ts.to_string(),
    });
}

async fn clear_cached_conversation_unread(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    latest_ts: &str,
) {
    if let Some(store) = workspace_store.as_ref() {
        if let Err(error) = store
            .clear_conversation_unread_state(channel_id, latest_ts)
            .await
        {
            crate::debug::log(
                "store",
                &format!("ConversationReadStoreFailed channel_id={channel_id} error={error:#}"),
            );
        }
    }
}

async fn store_conversation_unread_state(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    unread_state: SlackUnreadState,
    server_last_read: Option<&str>,
) -> bool {
    let Some(store) = workspace_store.as_ref() else {
        return unread_state.known;
    };
    match store
        .apply_conversation_unread_state(channel_id, unread_state, server_last_read)
        .await
    {
        Ok(applied) => applied,
        Err(error) => {
            crate::debug::log(
                "store",
                &format!("ConversationUnreadStoreFailed channel_id={channel_id} error={error:#}"),
            );
            false
        }
    }
}

async fn observe_thread_history(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    if let Err(error) = store.observe_thread_history(channel_id, messages).await {
        crate::debug::log(
            "store",
            &format!("ThreadCatalogStoreFailed error={error:#}"),
        );
        return;
    }
    load_cached_thread_catalog(events, workspace_store).await;
}

async fn observe_thread_page(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    root_ts: &str,
    messages: &[SlackMessage],
    complete: bool,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    if let Err(error) = store
        .observe_thread_page(channel_id, root_ts, messages, complete)
        .await
    {
        crate::debug::log(
            "store",
            &format!("ThreadCatalogStoreFailed error={error:#}"),
        );
        return;
    }
    load_cached_thread_catalog(events, workspace_store).await;
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

async fn load_cached_conversations(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_conversations().await {
        Ok(Some(conversations)) => {
            crate::debug::log(
                "runtime",
                &format!("CachedConversationsLoaded count={}", conversations.len()),
            );
            events.send_event(RuntimeEventKind::ConversationsLoaded(conversations));
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedConversationsLoadFailed error={error:#}"),
        ),
    }
}

async fn load_cached_thread_catalog(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };
    match store.load_thread_catalog().await {
        Ok(records) if !records.is_empty() => {
            events.send_event(RuntimeEventKind::ThreadCatalogLoaded(records));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedThreadCatalogLoadFailed error={error:#}"),
        ),
    }
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

async fn load_cached_history(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_history(channel_id).await {
        Ok(Some(messages)) => {
            let preview = recent_history_preview(messages);
            crate::debug::log(
                "runtime",
                &format!(
                    "CachedHistoryLoaded channel_id={channel_id} messages={}",
                    preview.len()
                ),
            );
            events.send_event(RuntimeEventKind::HistoryLoaded {
                channel_id: channel_id.to_string(),
                messages: preview,
                has_more: false,
                next_cursor: None,
                append_older: false,
                cached: true,
            });
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedHistoryLoadFailed channel_id={channel_id} error={error:#}"),
        ),
    }
}

async fn store_history(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_history(channel_id, messages).await {
        crate::debug::log(
            "runtime",
            &format!("CachedHistoryStoreFailed channel_id={channel_id} error={error:#}"),
        );
    }
}

async fn store_merged_history(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_merged_history(channel_id, messages).await {
        crate::debug::log(
            "runtime",
            &format!("CachedHistoryMergedStoreFailed channel_id={channel_id} error={error:#}"),
        );
    }
}

async fn load_cached_thread(
    events: &RuntimeEventSender,
    workspace_store: &Option<WorkspaceStore>,
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
            events.send_event(RuntimeEventKind::ThreadLoaded {
                channel_id: channel_id.to_string(),
                ts: thread_ts.to_string(),
                messages,
                has_more: false,
                next_cursor: None,
                append_older: false,
            });
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

async fn store_thread(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    thread_ts: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_thread(channel_id, thread_ts, messages).await {
        crate::debug::log(
            "runtime",
            &format!(
                "CachedThreadStoreFailed channel_id={channel_id} ts={thread_ts} error={error:#}"
            ),
        );
    }
}

fn require_slack(slack: &Option<SlackApi>) -> Result<&SlackApi> {
    slack.as_ref().context("No Slack workspace is available")
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
        }
    }

    fn unsolicited(&self, context: OperationContext) -> Self {
        Self {
            sender: self.sender.clone(),
            session: self.session,
            request: None,
            fallback: context,
        }
    }
}

impl EventSenderExt for RuntimeEventSender {
    fn send_status(&self, status: &str) {
        self.send_event(RuntimeEventKind::Status(status.to_string()));
    }

    fn send_failure(&self, error: &anyhow::Error) {
        crate::debug::log(
            "runtime",
            &format!(
                "RuntimeOperationFailed operation={:?} target={:?} error={error:#}",
                self.fallback.operation, self.fallback.target
            ),
        );
        self.send_event(RuntimeEventKind::Error(RuntimeFailure::from_error(error)));
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
    use std::future;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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

        let auth = RuntimeFailure::from_error(&auth);
        let storage = RuntimeFailure::from_error(&storage);

        assert_eq!(auth.category, RuntimeFailureCategory::Authentication);
        assert_eq!(auth.message, "Slack authentication failed. Sign in again.");
        assert_eq!(storage.category, RuntimeFailureCategory::Storage);
        assert_eq!(storage.message, "Conduit could not access its local data.");
        assert!(!storage.message.contains("secret cache path"));
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
                Some(RuntimeEventKind::SignedOut)
            ));
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
            let (sender, receiver) = mpsc::channel(2);
            let worker = tokio::spawn(persist_realtime_events(
                receiver,
                store.clone(),
                Some("U_SELF".into()),
                event_sender,
            ));
            for ts in ["1.0", "2.0"] {
                sender
                    .send(RealtimePersistenceEvent::Message(Box::new(
                        crate::socket_mode::SocketModeMessageEvent {
                            channel_id: "C1".into(),
                            message: SlackMessage {
                                ts: ts.into(),
                                user: Some("U_OTHER".into()),
                                text: Some(format!("message {ts}")),
                                ..Default::default()
                            },
                            kind: SocketModeMessageKind::Posted,
                        },
                    )))
                    .await
                    .unwrap();
            }
            drop(sender);
            worker.await.unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(history.len(), 2);
            let conversation = store.load_conversations().await.unwrap().unwrap().remove(0);
            assert_eq!(conversation.unread_activity_count(), 2);
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

            state
                .lock()
                .expect("runtime state lock poisoned")
                .replace_session(second_session);

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
    }

    #[test]
    fn conversation_refresh_runs_in_background() {
        assert_eq!(
            conversation_refresh_mode(),
            ConversationRefreshMode::Background
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
    fn cached_dm_user_ids_selects_only_known_direct_messages() {
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
            cached_dm_user_ids(&conversations, &user_cache),
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
    fn recent_history_preview_keeps_latest_page_only() {
        let count = CHANNEL_HISTORY_PAGE_LIMIT + 5;
        let messages = (0..count)
            .map(|index| SlackMessage {
                ts: format!("1710000{index:03}.000000"),
                text: Some(format!("message {index}")),
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let preview = recent_history_preview(messages);
        let first_ts = format!("1710000{:03}.000000", count - 1);
        let last_ts = format!("1710000{:03}.000000", count - CHANNEL_HISTORY_PAGE_LIMIT);

        assert_eq!(preview.len(), CHANNEL_HISTORY_PAGE_LIMIT);
        assert_eq!(
            preview.first().map(|message| message.ts.as_str()),
            Some(first_ts.as_str())
        );
        assert_eq!(
            preview.last().map(|message| message.ts.as_str()),
            Some(last_ts.as_str())
        );
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
    fn conversation_unread_refresh_candidates_prioritize_attention_and_cover_every_item() {
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

        let many = (0..75)
            .map(|index| channel(&format!("C{index}"), 0, None))
            .collect::<Vec<_>>();
        assert_eq!(conversation_unread_refresh_candidates(&many).len(), 75);
    }

    #[test]
    fn membership_reconciliation_preserves_enriched_unread_fields() {
        let mut cached = channel("C1", 5, Some("1710000000.000000"));
        cached
            .extra
            .insert("unread_count_display".to_string(), serde_json::json!(3));
        let fresh = SlackConversation {
            id: "C1".to_string(),
            name: Some("renamed".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };

        let reconciled = reconcile_conversation_snapshot(vec![cached], vec![fresh])
            .expect("snapshot should reconcile");

        assert_eq!(reconciled[0].name.as_deref(), Some("renamed"));
        assert_eq!(reconciled[0].unread_activity_count(), 5);
        assert_eq!(reconciled[0].unread_state().display_count, 3);
    }

    #[test]
    fn suspicious_empty_membership_snapshot_does_not_erase_cache() {
        let cached = vec![channel("C1", 0, None)];

        assert!(reconcile_conversation_snapshot(cached, Vec::new()).is_err());
        assert!(reconcile_conversation_snapshot(Vec::new(), Vec::new())
            .expect("an empty first workspace snapshot is valid")
            .is_empty());
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
}
