use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use sha2::{Digest, Sha256};

use crate::attention::AttentionPreferences;
use crate::huddles::state::HuddleCommand;
use crate::message_handoff::MessageControlHandle;
use crate::models::{SearchMessageLocation, SlackMessage, SlackUserStatus};
use crate::runtime::{RuntimeEventKind, RuntimeEventMeta};
use crate::slack::SlackMessageActionRequest;

pub const RUNTIME_EVENT_QUEUE_CAPACITY: usize = 256;
pub const RUNTIME_EVENT_PROGRESS_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NavigationSlot {
    Main,
    Thread,
}

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
        attachments_json: Option<String>,
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
pub struct SessionId(pub u64);

impl SessionId {
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(pub u64);

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
pub enum RuntimeAdmissionKind {
    Control,
    DurableAction,
    ReadMarker,
    Coalescible,
    Supersedable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConversationDiscoveryScope {
    Full,
    Channels,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UserLoadScope {
    Basic,
    Profile,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct OpaqueAdmissionTarget(pub [u8; 32]);

impl OpaqueAdmissionTarget {
    pub fn digest(parts: &[&str]) -> Self {
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeAdmissionKey {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeAdmissionPolicy {
    pub kind: RuntimeAdmissionKind,
    pub replacement_key: Option<RuntimeAdmissionKey>,
}

impl RuntimeAdmissionPolicy {
    pub fn control() -> Self {
        Self {
            kind: RuntimeAdmissionKind::Control,
            replacement_key: None,
        }
    }

    pub fn durable_action() -> Self {
        Self {
            kind: RuntimeAdmissionKind::DurableAction,
            replacement_key: None,
        }
    }

    pub fn read_marker() -> Self {
        Self {
            kind: RuntimeAdmissionKind::ReadMarker,
            replacement_key: None,
        }
    }

    pub fn coalescible(replacement_key: RuntimeAdmissionKey) -> Self {
        Self {
            kind: RuntimeAdmissionKind::Coalescible,
            replacement_key: Some(replacement_key),
        }
    }

    pub fn supersedable(replacement_key: RuntimeAdmissionKey) -> Self {
        Self {
            kind: RuntimeAdmissionKind::Supersedable,
            replacement_key: Some(replacement_key),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub struct RuntimeTraceFields {
    pub session: SessionId,
    pub request: RequestId,
    pub operation: &'static str,
    pub target: String,
    pub queue_kind: &'static str,
}

#[derive(Debug)]
pub struct RuntimeEvent {
    pub meta: RuntimeEventMeta,
    pub kind: RuntimeEventKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProgressKind {
    AttachmentDownload,
    FileUpload,
}

impl RuntimeEvent {
    pub fn progress_kind(&self) -> Option<RuntimeProgressKind> {
        match self.kind {
            RuntimeEventKind::AttachmentDownloadProgress { .. } => {
                Some(RuntimeProgressKind::AttachmentDownload)
            }
            RuntimeEventKind::FileUploadProgress { .. } => Some(RuntimeProgressKind::FileUpload),
            _ => None,
        }
    }

    pub fn replaces_progress(&self, queued: &Self) -> bool {
        self.meta == queued.meta && self.progress_kind() == queued.progress_kind()
    }

    pub fn completes_progress(&self, queued: &Self) -> bool {
        if self.meta != queued.meta {
            return false;
        }

        matches!(
            (&self.kind, queued.progress_kind()),
            (
                RuntimeEventKind::AttachmentDownloaded { .. },
                Some(RuntimeProgressKind::AttachmentDownload)
            ) | (
                RuntimeEventKind::FileUploaded(_),
                Some(RuntimeProgressKind::FileUpload)
            ) | (RuntimeEventKind::Error(_), Some(_))
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeEventMailboxSnapshot {
    pub admitted: u64,
    pub dequeued: u64,
    pub blocked: u64,
    pub closed: u64,
    pub depth: usize,
    pub peak_depth: usize,
    pub coalesced_progress: u64,
    pub dropped_progress: u64,
}

#[derive(Debug)]
pub struct RuntimeEventMailboxState {
    pub queue: VecDeque<RuntimeEvent>,
    pub capacity: usize,
    pub progress_capacity: usize,
    pub progress_depth: usize,
    pub next_reliable_ticket: u64,
    pub serving_reliable_ticket: u64,
    pub sender_count: usize,
    pub receiver_open: bool,
    pub metrics: RuntimeEventMailboxSnapshot,
}

impl RuntimeEventMailboxState {
    pub fn snapshot(&self) -> RuntimeEventMailboxSnapshot {
        RuntimeEventMailboxSnapshot {
            depth: self.queue.len(),
            ..self.metrics
        }
    }

    pub fn record_admitted(&mut self) {
        self.metrics.admitted = self.metrics.admitted.saturating_add(1);
        self.metrics.peak_depth = self.metrics.peak_depth.max(self.queue.len());
    }

    pub fn has_reliable_waiters(&self) -> bool {
        self.next_reliable_ticket != self.serving_reliable_ticket
    }

    pub fn reserve_reliable_ticket(&mut self) -> u64 {
        let ticket = self.next_reliable_ticket;
        self.next_reliable_ticket = self
            .next_reliable_ticket
            .checked_add(1)
            .expect("runtime event reliable ticket overflow");
        ticket
    }

    pub fn complete_reliable_ticket(&mut self, ticket: u64) {
        debug_assert_eq!(ticket, self.serving_reliable_ticket);
        self.serving_reliable_ticket = self
            .serving_reliable_ticket
            .checked_add(1)
            .expect("runtime event reliable ticket overflow");
        if self.serving_reliable_ticket == self.next_reliable_ticket {
            self.serving_reliable_ticket = 0;
            self.next_reliable_ticket = 0;
        }
    }

    pub fn evict_completed_progress(&mut self, terminal: &RuntimeEvent) {
        let original_depth = self.queue.len();
        self.queue
            .retain(|queued| !terminal.completes_progress(queued));
        let removed = original_depth - self.queue.len();
        self.progress_depth -= removed;
        self.metrics.coalesced_progress = self
            .metrics
            .coalesced_progress
            .saturating_add(removed as u64);
    }
}

#[derive(Debug)]
pub struct RuntimeEventMailboxInner {
    pub state: Mutex<RuntimeEventMailboxState>,
    pub available: tokio::sync::Notify,
    pub space: Condvar,
}

pub struct RuntimeEventMailboxSender {
    pub inner: Arc<RuntimeEventMailboxInner>,
}

impl std::fmt::Debug for RuntimeEventMailboxSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RuntimeEventMailboxSender").finish()
    }
}

impl Clone for RuntimeEventMailboxSender {
    fn clone(&self) -> Self {
        self.inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned")
            .sender_count += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for RuntimeEventMailboxSender {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned");
        state.sender_count -= 1;
        let closed = state.sender_count == 0;
        drop(state);
        if closed {
            // `notify_one` stores a permit if the receiver is between checking
            // sender_count and polling `notified`, preventing a lost EOF wake.
            self.inner.available.notify_one();
        }
    }
}

impl RuntimeEventMailboxSender {
    pub fn send(&self, event: RuntimeEvent) -> std::result::Result<(), Box<RuntimeEvent>> {
        let is_progress = event.progress_kind().is_some();
        let mut event = Some(event);
        let mut recorded_block = false;
        let mut reliable_ticket = None;

        loop {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("runtime event mailbox lock poisoned");
            if !state.receiver_open {
                state.metrics.closed = state.metrics.closed.saturating_add(1);
                return Err(Box::new(event.take().expect("runtime event missing")));
            }

            let pending = event.as_ref().expect("runtime event missing");
            if is_progress {
                if state.has_reliable_waiters() {
                    state.metrics.dropped_progress =
                        state.metrics.dropped_progress.saturating_add(1);
                    return Ok(());
                }
                if let Some(index) = state
                    .queue
                    .iter()
                    .position(|queued| pending.replaces_progress(queued))
                {
                    state.queue.remove(index);
                    state.progress_depth -= 1;
                    state.metrics.coalesced_progress =
                        state.metrics.coalesced_progress.saturating_add(1);
                } else if state.progress_depth >= state.progress_capacity
                    || state.queue.len() >= state.capacity
                {
                    state.metrics.dropped_progress =
                        state.metrics.dropped_progress.saturating_add(1);
                    return Ok(());
                }
            } else if reliable_ticket.is_none() && state.has_reliable_waiters() {
                reliable_ticket = Some(state.reserve_reliable_ticket());
            }

            if let Some(ticket) = reliable_ticket {
                if ticket != state.serving_reliable_ticket {
                    if !recorded_block {
                        state.metrics.blocked = state.metrics.blocked.saturating_add(1);
                        recorded_block = true;
                    }
                    state = wait_for_runtime_event_space(&self.inner.space, state);
                    drop(state);
                    continue;
                }
            }

            if !is_progress {
                state.evict_completed_progress(pending);
            }

            if state.queue.len() < state.capacity {
                state
                    .queue
                    .push_back(event.take().expect("runtime event missing"));
                if is_progress {
                    state.progress_depth += 1;
                }
                state.record_admitted();
                if let Some(ticket) = reliable_ticket {
                    state.complete_reliable_ticket(ticket);
                }
                drop(state);
                self.inner.available.notify_one();
                if reliable_ticket.is_some() {
                    self.inner.space.notify_all();
                }
                return Ok(());
            }

            if reliable_ticket.is_none() {
                reliable_ticket = Some(state.reserve_reliable_ticket());
            }
            if !recorded_block {
                state.metrics.blocked = state.metrics.blocked.saturating_add(1);
                recorded_block = true;
            }
            state = wait_for_runtime_event_space(&self.inner.space, state);
            drop(state);
        }
    }

    pub fn snapshot(&self) -> RuntimeEventMailboxSnapshot {
        self.inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned")
            .snapshot()
    }
}

pub fn wait_for_runtime_event_space<'a>(
    space: &Condvar,
    state: std::sync::MutexGuard<'a, RuntimeEventMailboxState>,
) -> std::sync::MutexGuard<'a, RuntimeEventMailboxState> {
    let wait = || {
        space
            .wait(state)
            .expect("runtime event mailbox lock poisoned")
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(wait)
        }
        // Production saturation happens only on the multi-thread runtime while
        // GLib drains independently. This direct wait is for the pre-runtime
        // startup thread and tests whose consumer runs on another thread; a
        // saturated current-thread runtime with an in-runtime consumer would
        // deadlock and is not a supported mailbox topology.
        _ => wait(),
    }
}

pub struct RuntimeEventReceiver {
    pub inner: Arc<RuntimeEventMailboxInner>,
}

impl RuntimeEventReceiver {
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        loop {
            let available = self.inner.available.notified();
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .expect("runtime event mailbox lock poisoned");
                if let Some(event) = state.queue.pop_front() {
                    if event.progress_kind().is_some() {
                        state.progress_depth -= 1;
                    }
                    state.metrics.dequeued = state.metrics.dequeued.saturating_add(1);
                    drop(state);
                    self.inner.space.notify_all();
                    return Some(event);
                }
                if state.sender_count == 0 {
                    return None;
                }
            }
            available.await;
        }
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> std::result::Result<RuntimeEvent, std::sync::mpsc::TryRecvError> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned");
        if let Some(event) = state.queue.pop_front() {
            if event.progress_kind().is_some() {
                state.progress_depth -= 1;
            }
            state.metrics.dequeued = state.metrics.dequeued.saturating_add(1);
            drop(state);
            self.inner.space.notify_all();
            return Ok(event);
        }
        if state.sender_count == 0 {
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        } else {
            Err(std::sync::mpsc::TryRecvError::Empty)
        }
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> RuntimeEventMailboxSnapshot {
        self.inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned")
            .snapshot()
    }
}

impl Drop for RuntimeEventReceiver {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("runtime event mailbox lock poisoned");
        state.receiver_open = false;
        state.queue.clear();
        state.progress_depth = 0;
        drop(state);
        self.inner.space.notify_all();
    }
}

pub fn runtime_event_channel() -> (RuntimeEventMailboxSender, RuntimeEventReceiver) {
    runtime_event_channel_with_capacity(
        RUNTIME_EVENT_QUEUE_CAPACITY,
        RUNTIME_EVENT_PROGRESS_CAPACITY,
    )
}

pub fn runtime_event_channel_with_capacity(
    capacity: usize,
    progress_capacity: usize,
) -> (RuntimeEventMailboxSender, RuntimeEventReceiver) {
    assert!(capacity > 0, "runtime event capacity must be positive");
    assert!(
        progress_capacity <= capacity,
        "runtime event progress capacity exceeds total capacity"
    );
    let inner = Arc::new(RuntimeEventMailboxInner {
        state: Mutex::new(RuntimeEventMailboxState {
            queue: VecDeque::with_capacity(capacity),
            capacity,
            progress_capacity,
            progress_depth: 0,
            next_reliable_ticket: 0,
            serving_reliable_ticket: 0,
            sender_count: 1,
            receiver_open: true,
            metrics: RuntimeEventMailboxSnapshot::default(),
        }),
        available: tokio::sync::Notify::new(),
        space: Condvar::new(),
    });

    (
        RuntimeEventMailboxSender {
            inner: inner.clone(),
        },
        RuntimeEventReceiver { inner },
    )
}
