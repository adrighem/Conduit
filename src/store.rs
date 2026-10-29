use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::lock::Mutex;
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::conversation_catalog::ConversationCatalog;
use crate::models::{
    slack_timestamp_is_after, SlackConversation, SlackMessage, SlackUser, SlackUserStatus,
};
use crate::slack::SidebarTheme;
use crate::slack_message_wire::normalize_cached_messages;
use crate::thread_catalog::{ThreadCatalog, ThreadRecord};
use crate::unread_ledger::{ConversationReadState, UnreadLedger};
use crate::workspace_pipeline::{
    same_message_identity, MessageMutationKind, StoreBatch, StoreChange, WorkspaceRevision,
};

pub(crate) const CACHE_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: u32 = 3;
const DATABASE_FILENAME: &str = "state.sqlite3";
const LEGACY_CONVERSATION_READ_KEYS: [&str; 5] = [
    "last_read",
    "unread_count",
    "unread_count_display",
    "unread_mentions",
    "has_unreads",
];
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;
/// Channels from the last completed discovery sweep.
const DISCOVERED_CHANNEL_KIND: &str = "discovered_channel";
/// Channels resolved on demand (`conversations.info`), such as archived ones
/// the sweep skips; sweeps never replace or drop these.
const RESOLVED_CHANNEL_KIND: &str = "resolved_channel";
/// Channels from a sweep still in progress; replaces the set on completion.
const DISCOVERY_PARTIAL_KIND: &str = "discovered_channel_partial";
const DISCOVERY_PROGRESS_KIND: &str = "channel_discovery_progress";
const DISCOVERY_PROGRESS_KEY: &str = "channels";
/// `sync_metadata` operation whose refresh time is the last completed sweep.
const CHANNEL_DISCOVERY_OPERATION: &str = "channel_discovery";
const ATTENTION_DELIVERY_KIND: &str = "attention_delivery";
const ATTENTION_DELIVERY_LEDGER_KEY: &str = "__ledger__";
const MAX_ATTENTION_DELIVERIES: usize = 512;
const STORE_WRITER_QUEUE_CAPACITY: usize = 64;
const STORE_READER_QUEUE_CAPACITY: usize = 32;
const STORE_READER_COUNT: usize = 2;
const STORE_MAINTENANCE_BATCH_LIMIT: usize = 50;
const STORE_MAINTENANCE_BATCH_WINDOW: Duration = Duration::from_millis(5);

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreErrorCategory {
    LocalIo,
    TemporarilyUnavailable,
    CorruptData,
    IncompatibleSchema,
    RejectedUpdate,
    Unexpected,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("{message}")]
    RejectedUpdate { message: String },
    #[error("workspace database schema {found} is newer than supported schema {supported}")]
    IncompatibleSchema { found: u32, supported: u32 },
    #[error("derived workspace cache is invalid: {message}")]
    InvalidDerivedCache { message: String },
    #[error("corrupt workspace item kind='{kind}' item_key='{item_key}': {message}")]
    CorruptItem {
        kind: String,
        item_key: String,
        message: String,
    },
    #[error("workspace store hub is closed")]
    HubClosed,
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    fn rejected_update(message: impl Into<String>) -> Self {
        Self::RejectedUpdate {
            message: message.into(),
        }
    }

    fn incompatible_schema(found: u32, supported: u32) -> Self {
        Self::IncompatibleSchema { found, supported }
    }

    fn invalid_derived_cache(message: impl Into<String>) -> Self {
        Self::InvalidDerivedCache {
            message: message.into(),
        }
    }

    fn corrupt_item(
        kind: impl Into<String>,
        item_key: impl Into<String>,
        message: impl std::fmt::Display,
    ) -> Self {
        Self::CorruptItem {
            kind: kind.into(),
            item_key: item_key.into(),
            message: message.to_string(),
        }
    }

    pub(crate) fn category(&self) -> StoreErrorCategory {
        match self {
            Self::RejectedUpdate { .. } => StoreErrorCategory::RejectedUpdate,
            Self::IncompatibleSchema { .. } => StoreErrorCategory::IncompatibleSchema,
            Self::InvalidDerivedCache { .. } => StoreErrorCategory::CorruptData,
            Self::CorruptItem { .. } => StoreErrorCategory::CorruptData,
            Self::HubClosed => StoreErrorCategory::TemporarilyUnavailable,
            Self::Database(error) => classify_database_error(error),
            Self::Io(_) => StoreErrorCategory::LocalIo,
            Self::Json(_) => StoreErrorCategory::CorruptData,
            Self::Other(error) => classify_wrapped_store_error(error),
        }
    }
}

type StoreWorkerValue = Box<dyn Any + Send>;
type StoreWorkerTask =
    Box<dyn FnOnce(&mut Connection) -> Result<StoreWorkerValue> + Send + 'static>;
type StoreMaintenanceTask =
    Box<dyn FnOnce(&Transaction<'_>) -> Result<StoreWorkerValue> + Send + 'static>;

struct StoreMaintenanceRequest {
    task: StoreMaintenanceTask,
    response: tokio::sync::oneshot::Sender<Result<StoreWorkerValue>>,
}

enum StoreWorkerRequest {
    Task {
        task: StoreWorkerTask,
        response: tokio::sync::oneshot::Sender<Result<StoreWorkerValue>>,
    },
    Maintenance(StoreMaintenanceRequest),
    Shutdown {
        response: tokio::sync::oneshot::Sender<()>,
    },
}

struct StoreHubInner {
    writer: tokio::sync::mpsc::Sender<StoreWorkerRequest>,
    readers: Vec<tokio::sync::mpsc::Sender<StoreWorkerRequest>>,
    next_reader: AtomicUsize,
    closed: AtomicBool,
    admission: tokio::sync::Mutex<()>,
    workers: std::sync::Mutex<Vec<std::thread::JoinHandle<Result<()>>>>,
    metrics: Arc<StoreMetrics>,
}

#[derive(Default)]
struct StoreMetrics {
    connections: AtomicU64,
    transactions: AtomicU64,
    changed_rows: AtomicU64,
    skipped_rows: AtomicU64,
    rolled_back_batches: AtomicU64,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct StoreMetricsSnapshot {
    pub(crate) connections: u64,
    transactions: u64,
    changed_rows: u64,
    skipped_rows: u64,
    pub(crate) rolled_back_batches: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreBatchExecution {
    Committed,
    Unchanged,
    SkippedStale,
}

/// Owns the bounded, persistent SQLite connections for one derived cache.
///
/// `WorkspaceStore` is migrated onto this compatibility seam incrementally so
/// callers can keep their focused APIs while per-operation connections retire.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct StoreHub {
    inner: Arc<StoreHubInner>,
}

#[allow(dead_code)]
impl StoreHub {
    pub(crate) async fn open(directory: PathBuf) -> Result<Self> {
        let metrics = Arc::new(StoreMetrics::default());
        let (writer, writer_startup, writer_worker) = spawn_store_worker(
            directory.clone(),
            StoreConnectionKind::Writer,
            STORE_WRITER_QUEUE_CAPACITY,
            Arc::clone(&metrics),
        );
        writer_startup.await.map_err(|_| StoreError::HubClosed)??;

        let mut readers = Vec::with_capacity(STORE_READER_COUNT);
        let mut workers = vec![writer_worker];
        for _ in 0..STORE_READER_COUNT {
            let (reader, startup, worker) = spawn_store_worker(
                directory.clone(),
                StoreConnectionKind::QueryOnly,
                STORE_READER_QUEUE_CAPACITY,
                Arc::clone(&metrics),
            );
            startup.await.map_err(|_| StoreError::HubClosed)??;
            readers.push(reader);
            workers.push(worker);
        }

        Ok(Self {
            inner: Arc::new(StoreHubInner {
                writer,
                readers,
                next_reader: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                admission: tokio::sync::Mutex::new(()),
                workers: std::sync::Mutex::new(workers),
                metrics,
            }),
        })
    }

    pub(crate) async fn write<T, F>(&self, task: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.dispatch(&self.inner.writer, task).await
    }

    pub(crate) async fn query<T, F>(&self, task: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let reader =
            self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % self.inner.readers.len();
        self.dispatch(&self.inner.readers[reader], task).await
    }

    pub(crate) async fn write_maintenance<T, F>(&self, task: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    {
        let admission = self.inner.admission.lock().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(StoreError::HubClosed);
        }
        let (response, result) = tokio::sync::oneshot::channel();
        self.inner
            .writer
            .send(StoreWorkerRequest::Maintenance(StoreMaintenanceRequest {
                task: Box::new(move |transaction| {
                    task(transaction).map(|value| Box::new(value) as StoreWorkerValue)
                }),
                response,
            }))
            .await
            .map_err(|_| StoreError::HubClosed)?;
        drop(admission);

        let value = result.await.map_err(|_| StoreError::HubClosed)??;
        value.downcast::<T>().map(|value| *value).map_err(|_| {
            StoreError::invalid_derived_cache("store worker returned an unexpected value type")
        })
    }

    pub(crate) async fn barrier(&self) -> Result<()> {
        self.write(|_| Ok(())).await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let admission = self.inner.admission.lock().await;
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let mut shutdowns = Vec::with_capacity(1 + self.inner.readers.len());
        for worker in std::iter::once(&self.inner.writer).chain(self.inner.readers.iter()) {
            let (response, shutdown) = tokio::sync::oneshot::channel();
            worker
                .send(StoreWorkerRequest::Shutdown { response })
                .await
                .map_err(|_| StoreError::HubClosed)?;
            shutdowns.push(shutdown);
        }
        drop(admission);

        for shutdown in shutdowns {
            shutdown.await.map_err(|_| StoreError::HubClosed)?;
        }
        let workers = std::mem::take(
            &mut *self
                .inner
                .workers
                .lock()
                .expect("store worker lock poisoned"),
        );
        for worker in workers {
            worker.join().map_err(|_| {
                StoreError::Other(anyhow::anyhow!("workspace store worker panicked"))
            })??;
        }
        Ok(())
    }

    async fn dispatch<T, F>(
        &self,
        worker: &tokio::sync::mpsc::Sender<StoreWorkerRequest>,
        task: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(StoreError::HubClosed);
        }
        let (response, result) = tokio::sync::oneshot::channel();
        worker
            .send(StoreWorkerRequest::Task {
                task: Box::new(move |connection| {
                    task(connection).map(|value| Box::new(value) as StoreWorkerValue)
                }),
                response,
            })
            .await
            .map_err(|_| StoreError::HubClosed)?;

        let value = result.await.map_err(|_| StoreError::HubClosed)??;
        value.downcast::<T>().map(|value| *value).map_err(|_| {
            StoreError::invalid_derived_cache("store worker returned an unexpected value type")
        })
    }

    pub(crate) fn metrics(&self) -> StoreMetricsSnapshot {
        StoreMetricsSnapshot {
            connections: self.inner.metrics.connections.load(Ordering::Relaxed),
            transactions: self.inner.metrics.transactions.load(Ordering::Relaxed),
            changed_rows: self.inner.metrics.changed_rows.load(Ordering::Relaxed),
            skipped_rows: self.inner.metrics.skipped_rows.load(Ordering::Relaxed),
            rolled_back_batches: self
                .inner
                .metrics
                .rolled_back_batches
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StoreConnectionKind {
    Writer,
    QueryOnly,
}

fn spawn_store_worker(
    directory: PathBuf,
    kind: StoreConnectionKind,
    capacity: usize,
    metrics: Arc<StoreMetrics>,
) -> (
    tokio::sync::mpsc::Sender<StoreWorkerRequest>,
    tokio::sync::oneshot::Receiver<Result<()>>,
    std::thread::JoinHandle<Result<()>>,
) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(capacity);
    let (startup, started) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("conduit-store".to_string())
        .spawn(move || {
            let connection = match kind {
                StoreConnectionKind::Writer => open_database(&directory),
                StoreConnectionKind::QueryOnly => open_query_database(&directory),
            };
            let mut connection = match connection {
                Ok(connection) => {
                    metrics.connections.fetch_add(1, Ordering::Relaxed);
                    let _ = startup.send(Ok(()));
                    connection
                }
                Err(error) => {
                    let _ = startup.send(Err(error));
                    return Ok(());
                }
            };

            let mut pending = None;
            loop {
                let request = match pending.take().or_else(|| receiver.blocking_recv()) {
                    Some(request) => request,
                    None => break,
                };
                match request {
                    StoreWorkerRequest::Task { task, response } => {
                        let before = connection.total_changes();
                        let result = task(&mut connection);
                        if kind == StoreConnectionKind::Writer {
                            let changed = connection.total_changes().saturating_sub(before);
                            if result.is_ok() {
                                record_store_work(
                                    &metrics,
                                    u64::from(changed > 0),
                                    changed,
                                    u64::from(changed == 0),
                                );
                            } else if changed > 0 {
                                metrics.rolled_back_batches.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        let _ = response.send(result);
                    }
                    StoreWorkerRequest::Maintenance(first) => {
                        pending =
                            run_maintenance_batch(&mut connection, first, &mut receiver, &metrics);
                    }
                    StoreWorkerRequest::Shutdown { response } => {
                        let _ = response.send(());
                        break;
                    }
                }
            }
            Ok(())
        })
        .expect("failed to spawn workspace store worker");
    (sender, started, worker)
}

fn run_maintenance_batch(
    connection: &mut Connection,
    first: StoreMaintenanceRequest,
    receiver: &mut tokio::sync::mpsc::Receiver<StoreWorkerRequest>,
    metrics: &StoreMetrics,
) -> Option<StoreWorkerRequest> {
    let deadline = std::time::Instant::now() + STORE_MAINTENANCE_BATCH_WINDOW;
    let mut batch = std::collections::VecDeque::from([first]);
    let mut pending = None;
    while batch.len() < STORE_MAINTENANCE_BATCH_LIMIT {
        match receiver.try_recv() {
            Ok(StoreWorkerRequest::Maintenance(request)) => batch.push_back(request),
            Ok(request) => {
                pending = Some(request);
                break;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
        }
    }

    let batch_len = batch.len() as u64;
    let before = connection.total_changes();
    let transaction = match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => {
            reject_maintenance_batch(batch, error.into());
            metrics.rolled_back_batches.fetch_add(1, Ordering::Relaxed);
            return pending;
        }
    };
    let mut completed = Vec::with_capacity(batch.len());
    while let Some(request) = batch.pop_front() {
        match (request.task)(&transaction) {
            Ok(value) => completed.push((request.response, value)),
            Err(error) => {
                let _ = transaction.rollback();
                let _ = request.response.send(Err(error));
                reject_completed_maintenance(completed);
                reject_maintenance_batch(
                    batch,
                    StoreError::rejected_update("store batch rolled back"),
                );
                metrics.rolled_back_batches.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(
                    target: "conduit::store",
                    event = "store_batch",
                    outcome = "rolled_back",
                    mutations = batch_len
                );
                return pending;
            }
        }
    }

    let changed = transaction.total_changes().saturating_sub(before);
    if changed == 0 {
        let _ = transaction.rollback();
        metrics.skipped_rows.fetch_add(batch_len, Ordering::Relaxed);
        crate::debug::pipeline_counters().record_sqlite_work(0, 0, batch_len);
        for (response, value) in completed {
            let _ = response.send(Ok(value));
        }
        tracing::trace!(
            target: "conduit::store",
            event = "store_batch",
            outcome = "unchanged",
            mutations = batch_len
        );
        return pending;
    }

    match transaction.commit() {
        Ok(()) => {
            record_store_work(metrics, 1, changed, 0);
            for (response, value) in completed {
                let _ = response.send(Ok(value));
            }
        }
        Err(error) => {
            let mut completed = completed.into_iter();
            if let Some((response, _)) = completed.next() {
                let _ = response.send(Err(error.into()));
            }
            reject_completed_maintenance(completed.collect());
            metrics.rolled_back_batches.fetch_add(1, Ordering::Relaxed);
        }
    }
    pending
}

fn reject_maintenance_batch(
    batch: std::collections::VecDeque<StoreMaintenanceRequest>,
    error: StoreError,
) {
    let mut batch = batch.into_iter();
    if let Some(request) = batch.next() {
        let _ = request.response.send(Err(error));
    }
    for request in batch {
        let _ = request
            .response
            .send(Err(StoreError::rejected_update("store batch rolled back")));
    }
}

fn reject_completed_maintenance(
    completed: Vec<(
        tokio::sync::oneshot::Sender<Result<StoreWorkerValue>>,
        StoreWorkerValue,
    )>,
) {
    for (response, _) in completed {
        let _ = response.send(Err(StoreError::rejected_update("store batch rolled back")));
    }
}

fn record_store_work(metrics: &StoreMetrics, transactions: u64, changed: u64, skipped: u64) {
    metrics
        .transactions
        .fetch_add(transactions, Ordering::Relaxed);
    metrics.changed_rows.fetch_add(changed, Ordering::Relaxed);
    metrics.skipped_rows.fetch_add(skipped, Ordering::Relaxed);
    crate::debug::pipeline_counters().record_sqlite_work(transactions, changed, skipped);
    tracing::trace!(
        target: "conduit::store",
        event = "store_batch",
        outcome = store_work_outcome(transactions),
        transactions,
        changed_rows = changed,
        skipped_rows = skipped
    );
}

fn store_work_outcome(transactions: u64) -> &'static str {
    if transactions == 0 {
        "unchanged"
    } else {
        "committed"
    }
}

fn classify_database_error(error: &rusqlite::Error) -> StoreErrorCategory {
    let rusqlite::Error::SqliteFailure(details, _) = error else {
        return StoreErrorCategory::Unexpected;
    };
    match details.code {
        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
            StoreErrorCategory::TemporarilyUnavailable
        }
        rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
            StoreErrorCategory::CorruptData
        }
        rusqlite::ErrorCode::CannotOpen
        | rusqlite::ErrorCode::DiskFull
        | rusqlite::ErrorCode::PermissionDenied
        | rusqlite::ErrorCode::ReadOnly
        | rusqlite::ErrorCode::SystemIoFailure => StoreErrorCategory::LocalIo,
        _ => StoreErrorCategory::Unexpected,
    }
}

fn classify_wrapped_store_error(error: &anyhow::Error) -> StoreErrorCategory {
    for source in error.chain() {
        if let Some(database) = source.downcast_ref::<rusqlite::Error>() {
            return classify_database_error(database);
        }
        if source.downcast_ref::<std::io::Error>().is_some() {
            return StoreErrorCategory::LocalIo;
        }
        if source.downcast_ref::<serde_json::Error>().is_some() {
            return StoreErrorCategory::CorruptData;
        }
    }
    StoreErrorCategory::Unexpected
}

#[derive(Clone)]
pub struct WorkspaceStore {
    directory: PathBuf,
    workspace_id: String,
    workspace_key: String,
    update_lock: Arc<Mutex<()>>,
    hub: Arc<tokio::sync::OnceCell<StoreHub>>,
    store_batch_revision: Arc<std::sync::Mutex<WorkspaceRevision>>,
    recovery_generation: Arc<AtomicU64>,
    conversation_repair_generation: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceBootstrap {
    pub(crate) workspace_id: String,
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) user_names: HashMap<String, String>,
    pub(crate) user_full_names: HashMap<String, String>,
    pub(crate) user_avatar_urls: HashMap<String, String>,
    pub(crate) user_search_aliases: HashMap<String, Vec<String>>,
    pub(crate) user_statuses: HashMap<String, SlackUserStatus>,
    pub(crate) thread_catalog: Vec<ThreadRecord>,
    pub(crate) custom_emojis: HashMap<String, String>,
    pub(crate) sidebar_theme: Option<SidebarTheme>,
    pub(crate) read_states: UnreadLedger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttentionObservationStatus {
    InvalidIdentity,
    AlreadyObserved,
    Accepted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AttentionDeliveryOutcome {
    pub(crate) observation: AttentionObservationStatus,
    pub(crate) notification_claimed: bool,
}

/// Where an interrupted discovery sweep resumes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ChannelDiscoveryProgress {
    pub(crate) cursor: String,
    pub(crate) pages: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChannelDiscoveryState {
    /// When the last full sweep completed.
    pub(crate) completed_at_ms: Option<i64>,
    pub(crate) progress: Option<ChannelDiscoveryProgress>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyncFreshness {
    pub(crate) refreshed_at_ms: Option<i64>,
    pub(crate) retry_count: u32,
    pub(crate) retry_after_ms: Option<i64>,
}

#[cfg(test)]
enum ConversationRowMutation<R> {
    Unchanged(R),
    Upsert(Box<SlackConversation>, R),
    Delete(R),
}

impl WorkspaceStore {
    pub fn new(directory: PathBuf, workspace_id: &str) -> Self {
        Self {
            directory,
            workspace_id: workspace_id.to_string(),
            workspace_key: cache_key(workspace_id),
            update_lock: Arc::new(Mutex::new(())),
            hub: Arc::new(tokio::sync::OnceCell::new()),
            store_batch_revision: Arc::new(std::sync::Mutex::new(WorkspaceRevision::INITIAL)),
            recovery_generation: Arc::new(AtomicU64::new(0)),
            conversation_repair_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn hub(&self) -> Result<&StoreHub> {
        let directory = self.directory.clone();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub
            .get_or_try_init(|| async move {
                let hub = StoreHub::open(directory.clone()).await?;
                hub.write(move |connection| {
                    migrate_legacy_workspace(connection, &directory, &workspace_key, &workspace_id)
                })
                .await?;
                Ok(hub)
            })
            .await
    }

    /// Executes one coordinator batch on the existing writer queue.
    ///
    /// The gate is strictly increasing rather than contiguous while compatibility
    /// surfaces still produce unsubmitted revisions. Migrated runtime paths must
    /// serialize reducer assignment and this submission.
    pub(crate) async fn execute_store_batch(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchExecution> {
        self.execute_store_batch_inner(batch, false).await
    }

    /// Rebuilds a reset cache from the coordinator's complete current
    /// projection. An equal revision is accepted because an intervening delta
    /// may already have reached the newly empty cache.
    pub(crate) async fn execute_store_repair_batch(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchExecution> {
        self.execute_store_batch_inner(batch, true).await
    }

    async fn execute_store_batch_inner(
        &self,
        batch: StoreBatch,
        accept_equal_revision: bool,
    ) -> Result<StoreBatchExecution> {
        let revision = batch.revision();
        let changes = batch.changes().to_vec();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let store_batch_revision = Arc::clone(&self.store_batch_revision);
        self.hub()
            .await?
            .write(move |connection| {
                let mut persisted_revision = store_batch_revision.lock().map_err(|_| {
                    StoreError::Other(anyhow::anyhow!("store batch revision lock poisoned"))
                })?;
                if revision < *persisted_revision
                    || (!accept_equal_revision && revision == *persisted_revision)
                {
                    return Ok(StoreBatchExecution::SkippedStale);
                }

                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                for change in changes {
                    match apply_store_change(&transaction, &workspace_key, &workspace_id, change) {
                        Ok(change_applied) => changed |= change_applied,
                        Err(error) => {
                            let _ = transaction.rollback();
                            return Err(error);
                        }
                    }
                }
                let outcome = if finish_sqlite_transaction(transaction, changed)? {
                    StoreBatchExecution::Committed
                } else {
                    StoreBatchExecution::Unchanged
                };
                *persisted_revision = revision;
                Ok(outcome)
            })
            .await
    }

    async fn query_or_reset<T, F>(&self, empty: T, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let hub = self.hub().await?;
        match hub.query(query).await {
            Err(StoreError::CorruptItem {
                kind,
                item_key,
                message,
            }) => {
                crate::debug::log(
                    "store",
                    &format!("CorruptWorkspaceItemDiscarded kind={kind} key={item_key}: {message}"),
                );
                let workspace_key = self.workspace_key.clone();
                let recovery_generation = Arc::clone(&self.recovery_generation);
                hub.write(move |connection| {
                    delete_sqlite_workspace_item(connection, &workspace_key, &kind, &item_key)?;
                    recovery_generation.fetch_add(1, Ordering::Release);
                    Ok(())
                })
                .await?;
                Ok(empty)
            }
            Err(error) if error.category() == StoreErrorCategory::CorruptData => {
                let workspace_key = self.workspace_key.clone();
                let store_batch_revision = Arc::clone(&self.store_batch_revision);
                let recovery_generation = Arc::clone(&self.recovery_generation);
                hub.write(move |connection| {
                    let mut persisted_revision = store_batch_revision.lock().map_err(|_| {
                        StoreError::Other(anyhow::anyhow!("store batch revision lock poisoned"))
                    })?;
                    reset_sqlite_workspace(connection, &workspace_key)?;
                    *persisted_revision = WorkspaceRevision::INITIAL;
                    recovery_generation.fetch_add(1, Ordering::Release);
                    Ok(())
                })
                .await?;
                Ok(empty)
            }
            result => result,
        }
    }

    pub(crate) fn recovery_generation(&self) -> u64 {
        self.recovery_generation.load(Ordering::Acquire)
    }

    pub(crate) fn conversation_cache_needs_repair(&self) -> bool {
        self.conversation_repair_generation.load(Ordering::Acquire) < self.recovery_generation()
    }

    pub(crate) fn mark_conversation_cache_repaired(&self, recovery_generation: u64) {
        self.conversation_repair_generation
            .fetch_max(recovery_generation, Ordering::AcqRel);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    async fn load_kind_map<T>(&self, kind: &'static str) -> Result<HashMap<String, T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset(HashMap::new(), move |connection| {
            load_sqlite_kind_map(connection, &workspace_key, kind)
        })
        .await
    }

    async fn store_kind_map<T>(
        &self,
        kind: &'static str,
        values: HashMap<String, T>,
        replace: bool,
    ) -> Result<()>
    where
        T: Serialize + Send + 'static,
    {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= if replace {
                    sync_sqlite_kind(&transaction, &workspace_key, kind, values)?
                } else {
                    let mut changed = false;
                    for (key, value) in values {
                        changed |=
                            upsert_sqlite_item(&transaction, &workspace_key, kind, &key, &value)?;
                    }
                    changed
                };
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    async fn update_thread_catalog<F, T>(&self, update: F) -> Result<(Vec<ThreadRecord>, T)>
    where
        F: FnOnce(&mut ThreadCatalog) -> T + Send + 'static,
        T: Send + 'static,
    {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                let records =
                    load_sqlite_kind_values(&transaction, &workspace_key, "thread_record")?;
                let mut catalog = ThreadCatalog::from_records(records);
                let result = update(&mut catalog);
                let records = catalog.into_records();
                changed |= sync_sqlite_kind(
                    &transaction,
                    &workspace_key,
                    "thread_record",
                    records.iter().cloned().map(|record| {
                        (
                            thread_key(&record.key.channel_id, &record.key.root_ts),
                            record,
                        )
                    }),
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok((records, result))
            })
            .await
    }

    pub(crate) async fn load_bootstrap(&self) -> Result<Option<WorkspaceBootstrap>> {
        let workspace_key = self.workspace_key.clone();
        let result = self
            .query_or_reset(None, move |connection| {
                load_sqlite_bootstrap(connection, &workspace_key)
            })
            .await;
        if let Err(error) = &result {
            crate::debug::log(
                "store",
                &format!(
                    "WorkspaceBootstrapReadFailed category={:?}",
                    error.category()
                ),
            );
        }
        result
    }

    #[allow(dead_code)]
    pub(crate) async fn load_sync_freshness(
        &self,
        operation: &str,
        target: &str,
    ) -> Result<Option<SyncFreshness>> {
        if operation.trim().is_empty() || target.trim().is_empty() {
            return Ok(None);
        }
        let workspace_key = self.workspace_key.clone();
        let operation = operation.to_string();
        let target = target.to_string();
        self.hub()
            .await?
            .query(move |connection| {
                connection
                    .query_row(
                        "SELECT refreshed_at_ms, retry_count, retry_after_ms
                         FROM sync_metadata
                         WHERE workspace_key = ?1 AND operation = ?2 AND target = ?3",
                        params![workspace_key, operation, target],
                        |row| {
                            Ok(SyncFreshness {
                                refreshed_at_ms: row.get(0)?,
                                retry_count: row.get(1)?,
                                retry_after_ms: row.get(2)?,
                            })
                        },
                    )
                    .optional()
                    .map_err(StoreError::from)
            })
            .await
    }

    #[allow(dead_code)]
    pub(crate) async fn store_sync_freshness(
        &self,
        operation: &str,
        target: &str,
        freshness: SyncFreshness,
    ) -> Result<()> {
        if operation.trim().is_empty() || target.trim().is_empty() {
            return Ok(());
        }
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let operation = operation.to_string();
        let target = target.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= transaction.execute(
                    "INSERT INTO sync_metadata(
                         workspace_key, operation, target,
                         refreshed_at_ms, retry_count, retry_after_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(workspace_key, operation, target) DO UPDATE SET
                         refreshed_at_ms = excluded.refreshed_at_ms,
                         retry_count = excluded.retry_count,
                         retry_after_ms = excluded.retry_after_ms
                     WHERE sync_metadata.refreshed_at_ms IS NOT excluded.refreshed_at_ms
                        OR sync_metadata.retry_count IS NOT excluded.retry_count
                        OR sync_metadata.retry_after_ms IS NOT excluded.retry_after_ms",
                    params![
                        workspace_key,
                        operation,
                        target,
                        freshness.refreshed_at_ms,
                        freshness.retry_count,
                        freshness.retry_after_ms
                    ],
                )? > 0;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    pub(crate) async fn store_sidebar_theme(&self, theme: &SidebarTheme) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let theme = theme.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= upsert_sqlite_item(
                    &transaction,
                    &workspace_key,
                    "sidebar_theme",
                    "theme",
                    &theme,
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    pub(crate) async fn validate_conversation_cache(&self) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset((), move |connection| {
            let _ = load_sqlite_kind_values::<SlackConversation>(
                connection,
                &workspace_key,
                "conversation",
            )?;
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub async fn load_conversations(&self) -> Result<Option<Vec<SlackConversation>>> {
        let workspace_key = self.workspace_key.clone();
        let conversations = self
            .query_or_reset(Vec::new(), move |connection| {
                load_sqlite_kind_values(connection, &workspace_key, "conversation")
            })
            .await?;
        Ok((!conversations.is_empty()).then_some(conversations))
    }

    /// Records the opaque workspace identity needed by desktop integrations,
    /// including when an older cache is opened while offline.
    pub async fn ensure_workspace_identity(&self) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, true)?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn store_conversations(&self, conversations: &[SlackConversation]) -> Result<()> {
        let values = conversations
            .iter()
            .filter(|conversation| !conversation.id.trim().is_empty())
            .map(conversation_for_cache)
            .map(|conversation| (conversation.id.clone(), conversation))
            .collect();
        self.store_kind_map("conversation", values, true).await
    }

    /// Merges one cached conversation without replacing the rest of the
    /// workspace snapshot.
    #[cfg(test)]
    pub async fn store_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        if conversation.id.trim().is_empty() {
            return Ok(());
        }

        let incoming = conversation.clone();
        self.mutate_conversation_row(&conversation.id, move |existing| {
            let mut catalog = ConversationCatalog::from_cached(existing);
            catalog.upsert_metadata(incoming);
            let conversation = catalog
                .conversations()
                .into_iter()
                .next()
                .expect("metadata upsert should produce a conversation");
            ConversationRowMutation::Upsert(Box::new(conversation), ())
        })
        .await
    }

    #[cfg(test)]
    pub async fn merge_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        self.store_conversation(conversation).await
    }

    /// Atomically claims a realtime delivery's notification identity, so a
    /// restart between classification and native delivery cannot turn one
    /// message into two notifications.
    #[cfg(test)]
    pub async fn accept_attention_delivery(
        &self,
        channel_id: &str,
        message_ts: &str,
        claim_notification: bool,
    ) -> Result<AttentionDeliveryOutcome> {
        self.accept_attention_delivery_for_message(channel_id, message_ts, claim_notification)
            .await
    }

    pub async fn accept_attention_delivery_for_message(
        &self,
        channel_id: &str,
        message_ts: &str,
        claim_notification: bool,
    ) -> Result<AttentionDeliveryOutcome> {
        let Some(identity) = attention_delivery_identity(channel_id, message_ts) else {
            return Ok(AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::InvalidIdentity,
                notification_claimed: false,
            });
        };

        let _guard = self.update_lock.lock().await;
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut ledger = load_sqlite_item::<Vec<String>>(
                    &transaction,
                    &workspace_key,
                    ATTENTION_DELIVERY_KIND,
                    ATTENTION_DELIVERY_LEDGER_KEY,
                )?
                .unwrap_or_default();
                if ledger.iter().any(|known| known == &identity) {
                    transaction.rollback()?;
                    return Ok(AttentionDeliveryOutcome {
                        observation: AttentionObservationStatus::AlreadyObserved,
                        notification_claimed: false,
                    });
                }
                let mut notification_claimed = false;
                let mut changed = false;
                if claim_notification {
                    ledger.push(identity);
                    if ledger.len() > MAX_ATTENTION_DELIVERIES {
                        ledger.drain(..ledger.len() - MAX_ATTENTION_DELIVERIES);
                    }
                    changed = ensure_sqlite_workspace(
                        &transaction,
                        &workspace_key,
                        &workspace_id,
                        false,
                    )?;
                    changed |= upsert_sqlite_item(
                        &transaction,
                        &workspace_key,
                        ATTENTION_DELIVERY_KIND,
                        ATTENTION_DELIVERY_LEDGER_KEY,
                        &ledger,
                    )?;
                    notification_claimed = true;
                }
                finish_sqlite_transaction(transaction, changed)?;
                Ok(AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::Accepted,
                    notification_claimed,
                })
            })
            .await
    }

    /// Removes one cached conversation without disturbing other catalog data.
    #[cfg(test)]
    pub async fn remove_conversation(&self, channel_id: &str) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        self.mutate_conversation_row(channel_id, |conversation| {
            if conversation.is_some() {
                ConversationRowMutation::Delete(true)
            } else {
                ConversationRowMutation::Unchanged(false)
            }
        })
        .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_user_names(&self) -> Result<HashMap<String, String>> {
        self.load_kind_map("user_name").await
    }

    #[cfg(test)]
    pub async fn store_user_name(&self, user_id: &str, display_name: &str) -> Result<()> {
        let mut names = HashMap::new();
        names.insert(user_id.to_string(), display_name.to_string());
        self.store_user_names(&names).await
    }

    #[cfg(test)]
    pub async fn store_user_names(&self, user_names: &HashMap<String, String>) -> Result<()> {
        let values = user_names
            .iter()
            .filter(|(user_id, display_name)| {
                !user_id.trim().is_empty() && !display_name.trim().is_empty()
            })
            .map(|(user_id, display_name)| (user_id.clone(), display_name.clone()))
            .collect();
        self.store_kind_map("user_name", values, false).await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_user_full_names(&self) -> Result<HashMap<String, String>> {
        self.load_kind_map("user_full_name").await
    }

    #[cfg(test)]
    pub async fn store_user_full_names(
        &self,
        user_full_names: &HashMap<String, String>,
    ) -> Result<()> {
        let values = user_full_names
            .iter()
            .filter(|(user_id, full_name)| {
                !user_id.trim().is_empty() && !full_name.trim().is_empty()
            })
            .map(|(user_id, full_name)| (user_id.clone(), full_name.clone()))
            .collect();
        self.store_kind_map("user_full_name", values, false).await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_user_avatar_urls(&self) -> Result<HashMap<String, String>> {
        self.load_kind_map("user_avatar_url").await
    }

    #[cfg(test)]
    pub async fn store_user_avatar_urls(
        &self,
        avatar_urls: &HashMap<String, String>,
    ) -> Result<()> {
        let values = avatar_urls
            .iter()
            .filter(|(user_id, url)| !user_id.trim().is_empty() && !url.trim().is_empty())
            .map(|(user_id, url)| (user_id.clone(), url.clone()))
            .collect();
        self.store_kind_map("user_avatar_url", values, false).await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_user_search_aliases(&self) -> Result<HashMap<String, Vec<String>>> {
        self.load_kind_map("user_aliases").await
    }

    #[cfg(test)]
    pub async fn store_user_search_aliases(
        &self,
        aliases: &HashMap<String, Vec<String>>,
    ) -> Result<()> {
        let values = aliases
            .iter()
            .filter(|(user_id, aliases)| {
                !user_id.trim().is_empty() && aliases.iter().any(|alias| !alias.trim().is_empty())
            })
            .map(|(user_id, aliases)| (user_id.clone(), aliases.clone()))
            .collect();
        self.store_kind_map("user_aliases", values, true).await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_user_statuses(&self) -> Result<HashMap<String, SlackUserStatus>> {
        self.load_kind_map("user_status").await
    }

    #[cfg(test)]
    pub async fn store_user_status(
        &self,
        user_id: &str,
        status: Option<SlackUserStatus>,
    ) -> Result<()> {
        if user_id.trim().is_empty() {
            return Ok(());
        }
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let user_id = user_id.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= match status {
                    Some(status) => upsert_sqlite_item(
                        &transaction,
                        &workspace_key,
                        "user_status",
                        &user_id,
                        &status,
                    )?,
                    None => {
                        transaction.execute(
                            "DELETE FROM workspace_items
                             WHERE workspace_key = ?1 AND kind = 'user_status' AND item_key = ?2",
                            params![workspace_key, user_id],
                        )? > 0
                    }
                };
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load_custom_emojis(&self) -> Result<HashMap<String, String>> {
        self.load_kind_map("custom_emoji").await
    }

    #[allow(dead_code)]
    pub async fn store_custom_emojis(&self, emojis: &HashMap<String, String>) -> Result<()> {
        self.store_kind_map("custom_emoji", emojis.clone(), true)
            .await
    }

    /// Channels from the last completed discovery sweep, overlaid with the
    /// pages of a sweep still in progress and any on-demand lookups. Never
    /// drops a channel, so a partial sweep only ever adds.
    pub(crate) async fn load_discovered_channels(&self) -> Result<Vec<SlackConversation>> {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset(Vec::new(), move |connection| {
            let mut channels = load_sqlite_kind_map::<SlackConversation>(
                connection,
                &workspace_key,
                RESOLVED_CHANNEL_KIND,
            )?;
            for kind in [DISCOVERED_CHANNEL_KIND, DISCOVERY_PARTIAL_KIND] {
                channels.extend(load_sqlite_kind_map::<SlackConversation>(
                    connection,
                    &workspace_key,
                    kind,
                )?);
            }
            let mut channels = channels.into_values().collect::<Vec<_>>();
            channels.sort_by_key(|channel| channel.display_name().to_lowercase());
            Ok(channels)
        })
        .await
    }

    /// Adds channels resolved on demand (`conversations.info`) to the
    /// discovered set; sweeps neither replace nor clear them.
    pub(crate) async fn upsert_discovered_channels(
        &self,
        channels: Vec<SlackConversation>,
    ) -> Result<()> {
        let values = channels
            .into_iter()
            .map(|channel| (channel.id.clone(), channel))
            .collect();
        self.store_kind_map(RESOLVED_CHANNEL_KIND, values, false)
            .await
    }

    pub(crate) async fn load_channel_discovery_state(&self) -> Result<ChannelDiscoveryState> {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset(ChannelDiscoveryState::default(), move |connection| {
            let progress = load_sqlite_item::<ChannelDiscoveryProgress>(
                connection,
                &workspace_key,
                DISCOVERY_PROGRESS_KIND,
                DISCOVERY_PROGRESS_KEY,
            )?;
            let completed_at_ms = connection
                .query_row(
                    "SELECT refreshed_at_ms FROM sync_metadata
                     WHERE workspace_key = ?1 AND operation = ?2 AND target = ?3",
                    params![workspace_key, CHANNEL_DISCOVERY_OPERATION, "workspace"],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            Ok(ChannelDiscoveryState {
                completed_at_ms,
                progress,
            })
        })
        .await
    }

    /// Persists one batch of a running sweep plus its resume point, or clears
    /// the resume point when `progress` is `None`. The batch is merged into
    /// the partial set, so earlier results survive an interruption.
    pub(crate) async fn save_discovery_progress(
        &self,
        batch: Vec<SlackConversation>,
        progress: Option<ChannelDiscoveryProgress>,
    ) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                for channel in &batch {
                    changed |= upsert_sqlite_item(
                        &transaction,
                        &workspace_key,
                        DISCOVERY_PARTIAL_KIND,
                        &channel.id,
                        channel,
                    )?;
                }
                changed |= match &progress {
                    Some(progress) => upsert_sqlite_item(
                        &transaction,
                        &workspace_key,
                        DISCOVERY_PROGRESS_KIND,
                        DISCOVERY_PROGRESS_KEY,
                        progress,
                    )?,
                    None => {
                        delete_sqlite_kind(&transaction, &workspace_key, DISCOVERY_PROGRESS_KIND)?
                    }
                };
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    /// Finishes a sweep: the accumulated pages replace the stored channel set
    /// (dropping channels that were archived or removed), the resume point is
    /// cleared and the completion time recorded, all in one transaction.
    pub(crate) async fn complete_discovery_sweep(
        &self,
        batch: Vec<SlackConversation>,
        completed_at_ms: i64,
    ) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                for channel in &batch {
                    upsert_sqlite_item(
                        &transaction,
                        &workspace_key,
                        DISCOVERY_PARTIAL_KIND,
                        &channel.id,
                        channel,
                    )?;
                }
                let swept = load_sqlite_kind_map::<SlackConversation>(
                    &transaction,
                    &workspace_key,
                    DISCOVERY_PARTIAL_KIND,
                )?;
                sync_sqlite_kind(&transaction, &workspace_key, DISCOVERED_CHANNEL_KIND, swept)?;
                delete_sqlite_kind(&transaction, &workspace_key, DISCOVERY_PARTIAL_KIND)?;
                delete_sqlite_kind(&transaction, &workspace_key, DISCOVERY_PROGRESS_KIND)?;
                transaction.execute(
                    "INSERT INTO sync_metadata(
                         workspace_key, operation, target,
                         refreshed_at_ms, retry_count, retry_after_ms
                     ) VALUES (?1, ?2, 'workspace', ?3, 0, NULL)
                     ON CONFLICT(workspace_key, operation, target) DO UPDATE SET
                         refreshed_at_ms = excluded.refreshed_at_ms,
                         retry_count = 0,
                         retry_after_ms = NULL",
                    params![workspace_key, CHANNEL_DISCOVERY_OPERATION, completed_at_ms],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn load_history(&self, channel_id: &str) -> Result<Option<Vec<SlackMessage>>> {
        let workspace_key = self.workspace_key.clone();
        let channel_id = channel_id.to_string();
        Ok(self
            .query_or_reset(None, move |connection| {
                load_sqlite_item::<Vec<SlackMessage>>(
                    connection,
                    &workspace_key,
                    "channel_history",
                    &channel_id,
                )
            })
            .await?
            .map(normalize_cached_messages)
            .map(channel_timeline_messages)
            .filter(|messages| !messages.is_empty()))
    }

    #[cfg(test)]
    pub async fn store_history(&self, channel_id: &str, messages: &[SlackMessage]) -> Result<()> {
        self.store_merged_history(channel_id, messages).await
    }

    #[cfg(test)]
    pub async fn store_merged_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        if channel_id.trim().is_empty() {
            return Ok(());
        }
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let channel_id = channel_id.to_string();
        let messages = normalize_cached_messages(messages.to_vec());
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing = normalize_cached_messages(
                    load_sqlite_item::<Vec<SlackMessage>>(
                        &transaction,
                        &workspace_key,
                        "channel_history",
                        &channel_id,
                    )?
                    .unwrap_or_default(),
                );
                let merged = merge_channel_history_pages(&existing, &messages);
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= upsert_sqlite_item(
                    &transaction,
                    &workspace_key,
                    "channel_history",
                    &channel_id,
                    &merged,
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(())
            })
            .await
    }

    pub async fn load_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> Result<Option<Vec<SlackMessage>>> {
        let key = thread_key(channel_id, thread_ts);
        let workspace_key = self.workspace_key.clone();
        Ok(self
            .query_or_reset(None, move |connection| {
                load_sqlite_item::<Vec<SlackMessage>>(
                    connection,
                    &workspace_key,
                    "thread_replies",
                    &key,
                )
            })
            .await?
            .map(normalize_cached_messages)
            .filter(|messages| !messages.is_empty()))
    }

    #[cfg(test)]
    pub async fn store_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        self.store_merged_thread(channel_id, thread_ts, messages)
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub async fn store_merged_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
    ) -> Result<Vec<SlackMessage>> {
        let key = thread_key(channel_id, thread_ts);
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let messages = normalize_cached_messages(messages.to_vec());
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing = normalize_cached_messages(
                    load_sqlite_item::<Vec<SlackMessage>>(
                        &transaction,
                        &workspace_key,
                        "thread_replies",
                        &key,
                    )?
                    .unwrap_or_default(),
                );
                let merged = merge_history_pages(&existing, &messages);
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= upsert_sqlite_item(
                    &transaction,
                    &workspace_key,
                    "thread_replies",
                    &key,
                    &merged,
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(merged)
            })
            .await
    }

    #[allow(dead_code)]
    pub async fn load_thread_catalog(&self) -> Result<Vec<ThreadRecord>> {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset(Vec::new(), move |connection| {
            load_sqlite_kind_values(connection, &workspace_key, "thread_record")
        })
        .await
    }

    #[allow(dead_code)]
    pub async fn store_thread_catalog(&self, records: &[ThreadRecord]) -> Result<()> {
        let records = records.to_vec();
        self.update_thread_catalog(move |catalog| {
            *catalog = ThreadCatalog::from_records(records);
        })
        .await
        .map(|_| ())
    }

    #[cfg(test)]
    async fn update_conversation(
        &self,
        channel_id: &str,
        update: impl FnOnce(&mut SlackConversation) + Send + 'static,
    ) -> Result<bool> {
        self.mutate_conversation_row(channel_id, move |conversation| {
            let Some(mut conversation) = conversation else {
                return ConversationRowMutation::Unchanged(false);
            };
            update(&mut conversation);
            ConversationRowMutation::Upsert(Box::new(conversation), true)
        })
        .await
    }

    #[cfg(test)]
    async fn mutate_conversation_row<R, F>(&self, channel_id: &str, update: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(Option<SlackConversation>) -> ConversationRowMutation<R> + Send + 'static,
    {
        // Startup and realtime sync can apply thousands of isolated conversation patches.
        // Keep those mutations row-scoped instead of rebuilding every cached workspace item.
        let _guard = self.update_lock.lock().await;
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let channel_id = channel_id.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing = load_sqlite_conversation(&transaction, &workspace_key, &channel_id)?;
                match update(existing) {
                    ConversationRowMutation::Unchanged(result) => {
                        transaction.rollback()?;
                        Ok(result)
                    }
                    ConversationRowMutation::Upsert(conversation, result) => {
                        let changed = upsert_sqlite_conversation(
                            &transaction,
                            &workspace_key,
                            &workspace_id,
                            &conversation,
                        )?;
                        finish_sqlite_transaction(transaction, changed)?;
                        Ok(result)
                    }
                    ConversationRowMutation::Delete(result) => {
                        let changed = transaction.execute(
                            "DELETE FROM workspace_items
                             WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                            params![workspace_key, channel_id],
                        )? > 0;
                        finish_sqlite_transaction(transaction, changed)?;
                        Ok(result)
                    }
                }
            })
            .await
    }

    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.directory.join(format!("{}.json", self.workspace_key))
    }

    #[cfg(test)]
    fn database_path(&self) -> PathBuf {
        database_path(&self.directory)
    }

    #[cfg(test)]
    pub(crate) async fn install_conversation_batch_failure_trigger(&self) -> Result<()> {
        self.install_conversation_batch_failure_trigger_matching(None)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn install_conversation_batch_failure_trigger_for(
        &self,
        channel_id: &str,
    ) -> Result<()> {
        if channel_id.is_empty()
            || !channel_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err(StoreError::rejected_update(
                "test failure channel id must be ASCII alphanumeric",
            ));
        }
        self.install_conversation_batch_failure_trigger_matching(Some(channel_id))
            .await
    }

    #[cfg(test)]
    async fn install_conversation_batch_failure_trigger_matching(
        &self,
        channel_id: Option<&str>,
    ) -> Result<()> {
        let target = channel_id
            .map(|channel_id| format!(" AND NEW.item_key = '{channel_id}'"))
            .unwrap_or_default();
        self.hub()
            .await?
            .write(move |connection| {
                connection.execute_batch(&format!(
                    "CREATE TEMP TRIGGER conduit_test_fail_conversation_batch
                     BEFORE INSERT ON workspace_items
                     WHEN NEW.kind = 'conversation'{target}
                     BEGIN
                         SELECT RAISE(ABORT, 'injected conversation batch failure');
                     END;"
                ))?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_conversation_batch_failure_trigger(&self) -> Result<()> {
        self.hub()
            .await?
            .write(|connection| {
                connection.execute_batch(
                    "DROP TRIGGER IF EXISTS conduit_test_fail_conversation_batch;",
                )?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn install_history_batch_failure_trigger_for(
        &self,
        channel_id: &str,
    ) -> Result<()> {
        if channel_id.is_empty()
            || !channel_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Err(StoreError::rejected_update(
                "test failure channel id must be ASCII alphanumeric",
            ));
        }
        let channel_id = channel_id.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                connection.execute_batch(&format!(
                    "CREATE TEMP TRIGGER conduit_test_fail_history_batch
                     BEFORE INSERT ON workspace_items
                     WHEN NEW.kind = 'channel_history' AND NEW.item_key = '{channel_id}'
                     BEGIN
                         SELECT RAISE(ABORT, 'injected history batch failure');
                     END;"
                ))?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_history_batch_failure_trigger(&self) -> Result<()> {
        self.hub()
            .await?
            .write(|connection| {
                connection
                    .execute_batch("DROP TRIGGER IF EXISTS conduit_test_fail_history_batch;")?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn occupy_writer_until(
        &self,
        started: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Result<()> {
        self.hub()
            .await?
            .write(move |_| {
                let _ = started.send(());
                release.recv().map_err(|_| {
                    StoreError::Other(anyhow::anyhow!("test writer release was dropped"))
                })?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn corrupt_conversation_payload(&self, channel_id: &str) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let channel_id = channel_id.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let changed = connection.execute(
                    "UPDATE workspace_items SET payload_json = '{'
                     WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                    params![workspace_key, channel_id],
                )?;
                if changed == 0 {
                    return Err(StoreError::rejected_update(
                        "test conversation payload was not found",
                    ));
                }
                Ok(())
            })
            .await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyWorkspaceState {
    version: u32,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    conversations: Vec<SlackConversation>,
    #[serde(default)]
    user_names: HashMap<String, String>,
    #[serde(default)]
    user_full_names: HashMap<String, String>,
    #[serde(default)]
    user_avatar_urls: HashMap<String, String>,
    #[serde(default)]
    user_search_aliases: HashMap<String, Vec<String>>,
    #[serde(default)]
    user_statuses: HashMap<String, SlackUserStatus>,
    #[serde(default)]
    channel_histories: HashMap<String, Vec<SlackMessage>>,
    #[serde(default)]
    thread_replies: HashMap<String, Vec<SlackMessage>>,
    #[serde(default)]
    thread_catalog: Vec<ThreadRecord>,
    #[serde(default)]
    custom_emojis: HashMap<String, String>,
    #[serde(default)]
    attention_deliveries: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct SearchProviderState {
    pub(crate) workspace_id: String,
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) user_names: HashMap<String, String>,
    pub(crate) user_full_names: HashMap<String, String>,
    pub(crate) user_search_aliases: HashMap<String, Vec<String>>,
}

pub(crate) fn load_active_search_state(directory: &Path) -> Result<Option<SearchProviderState>> {
    if !database_path(directory).exists() {
        if legacy_active_workspace_candidate(directory)?.is_none() {
            return Ok(None);
        }
        let mut connection = open_database(directory)?;
        migrate_legacy_active_workspace(&mut connection, directory)?;
    }

    let connection = match open_query_database(directory) {
        Ok(connection) => connection,
        Err(error) if error.category() == StoreErrorCategory::CorruptData => {
            recreate_derived_cache(directory)?;
            let _ = open_database(directory)?;
            return Ok(None);
        }
        Err(_) if !database_path(directory).exists() => return Ok(None),
        Err(error) => return Err(error),
    };

    let active_query = connection
        .query_row(
            "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional();
    let workspace_key = match active_query {
        Ok(key) => key.flatten(),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg))) if msg.contains("no such table") => {
            None
        }
        Err(error) => {
            let store_err: StoreError = error.into();
            if store_err.category() == StoreErrorCategory::CorruptData {
                drop(connection);
                recreate_derived_cache(directory)?;
                let _ = open_database(directory)?;
                return Ok(None);
            }
            return Err(store_err);
        }
    };

    let workspace_key = match workspace_key {
        Some(key) => key,
        None => {
            drop(connection);
            if legacy_active_workspace_candidate(directory)?.is_none() {
                return Ok(None);
            }
            let mut rw_connection = open_database(directory)?;
            migrate_legacy_active_workspace(&mut rw_connection, directory)?;
            let Some(key) = rw_connection
                .query_row(
                    "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
            else {
                return Ok(None);
            };
            return match load_sqlite_search_state(&rw_connection, &key) {
                Err(error) if error.category() == StoreErrorCategory::CorruptData => {
                    drop(rw_connection);
                    recreate_derived_cache(directory)?;
                    let _ = open_database(directory)?;
                    Ok(None)
                }
                result => result,
            };
        }
    };

    match load_sqlite_search_state(&connection, &workspace_key) {
        Err(error) if error.category() == StoreErrorCategory::CorruptData => {
            drop(connection);
            recreate_derived_cache(directory)?;
            let _ = open_database(directory)?;
            Ok(None)
        }
        result => result,
    }
}

pub(crate) fn clear_active_workspace(directory: &Path) -> Result<()> {
    if !database_path(directory).exists() {
        let _ = std::fs::remove_file(directory.join("active-workspace"));
        return Ok(());
    }
    let connection = open_database(directory)?;
    connection.execute(
        "UPDATE app_state SET active_workspace_key = NULL WHERE singleton = 1",
        [],
    )?;
    let _ = std::fs::remove_file(directory.join("active-workspace"));
    Ok(())
}

fn database_path(directory: &Path) -> PathBuf {
    directory.join(DATABASE_FILENAME)
}

fn open_database(directory: &Path) -> Result<Connection> {
    std::fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create state cache directory {}",
            directory.display()
        )
    })?;
    match open_database_once(directory) {
        Err(error) if error.category() == StoreErrorCategory::CorruptData => {
            recreate_derived_cache(directory)?;
            open_database_once(directory)
        }
        result => result,
    }
}

fn open_database_once(directory: &Path) -> Result<Connection> {
    let connection = Connection::open(database_path(directory)).with_context(|| {
        format!(
            "failed to open workspace database in {}",
            directory.display()
        )
    })?;
    crate::debug::pipeline_counters().record_sqlite_connections(1);
    connection.busy_timeout(Duration::from_secs(2))?;
    let schema_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
    if schema_version > DATABASE_SCHEMA_VERSION {
        return Err(StoreError::incompatible_schema(
            schema_version,
            DATABASE_SCHEMA_VERSION,
        ));
    }
    if let Err(error) = connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS workspaces (
             workspace_key TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS app_state (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             active_workspace_key TEXT REFERENCES workspaces(workspace_key)
         );
         INSERT OR IGNORE INTO app_state(singleton, active_workspace_key) VALUES (1, NULL);
         CREATE TABLE IF NOT EXISTS workspace_items (
             workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
             kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             PRIMARY KEY (workspace_key, kind, item_key)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS sync_metadata (
             workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
             operation TEXT NOT NULL,
             target TEXT NOT NULL,
             refreshed_at_ms INTEGER,
             retry_count INTEGER NOT NULL DEFAULT 0,
             retry_after_ms INTEGER,
             PRIMARY KEY (workspace_key, operation, target)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS conversation_read_state (
             workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
             channel_id TEXT NOT NULL,
             last_read TEXT,
             server_latest TEXT,
             server_mention_count INTEGER NOT NULL DEFAULT 0,
             server_unread_count INTEGER NOT NULL DEFAULT 0,
             server_has_unreads INTEGER NOT NULL DEFAULT 0,
             live_mentions_json TEXT NOT NULL DEFAULT '[]',
             live_latest_unread TEXT,
             PRIMARY KEY (workspace_key, channel_id)
         ) WITHOUT ROWID;
         PRAGMA user_version = {DATABASE_SCHEMA_VERSION};"
    )) {
        if schema_version < DATABASE_SCHEMA_VERSION {
            return Err(StoreError::invalid_derived_cache(format!(
                "schema migration from v{schema_version} failed: {error}"
            )));
        }
        return Err(error.into());
    }
    validate_schema_v2(&connection)?;
    Ok(connection)
}

fn open_query_database(directory: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        database_path(directory),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    crate::debug::pipeline_counters().record_sqlite_connections(1);
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(connection)
}

fn validate_schema_v2(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(sync_metadata)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = [
        "workspace_key",
        "operation",
        "target",
        "refreshed_at_ms",
        "retry_count",
        "retry_after_ms",
    ];
    if columns.iter().map(String::as_str).ne(expected) {
        return Err(StoreError::invalid_derived_cache(
            "schema-v2 sync metadata columns do not match",
        ));
    }
    Ok(())
}

fn recreate_derived_cache(directory: &Path) -> Result<()> {
    let database = database_path(directory);
    for path in [
        database.clone(),
        sqlite_sidecar_path(&database, "-wal"),
        sqlite_sidecar_path(&database, "-shm"),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn load_sqlite_bootstrap(
    connection: &Connection,
    workspace_key: &str,
) -> Result<Option<WorkspaceBootstrap>> {
    let workspace_id = connection
        .query_row(
            "SELECT workspace_id FROM workspaces WHERE workspace_key = ?1",
            [workspace_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };

    Ok(Some(WorkspaceBootstrap {
        workspace_id,
        conversations: load_sqlite_kind_values(connection, workspace_key, "conversation")?,
        user_names: load_sqlite_kind_map(connection, workspace_key, "user_name")?,
        user_full_names: load_sqlite_kind_map(connection, workspace_key, "user_full_name")?,
        user_avatar_urls: load_sqlite_kind_map(connection, workspace_key, "user_avatar_url")?,
        user_search_aliases: load_sqlite_kind_map(connection, workspace_key, "user_aliases")?,
        user_statuses: load_sqlite_kind_map(connection, workspace_key, "user_status")?,
        thread_catalog: load_sqlite_kind_values(connection, workspace_key, "thread_record")?,
        custom_emojis: load_sqlite_kind_map(connection, workspace_key, "custom_emoji")?,
        sidebar_theme: load_sqlite_item(connection, workspace_key, "sidebar_theme", "theme")?,
        read_states: load_sqlite_read_states(connection, workspace_key)?,
    }))
}

fn load_sqlite_read_states(connection: &Connection, workspace_key: &str) -> Result<UnreadLedger> {
    let mut statement = connection.prepare(
        "SELECT channel_id, last_read, server_latest, server_mention_count,
                server_unread_count, server_has_unreads, live_mentions_json, live_latest_unread
         FROM conversation_read_state WHERE workspace_key = ?1 ORDER BY channel_id",
    )?;
    let rows = statement.query_map([workspace_key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (channel_id, last_read, server_latest, mentions, unreads, has_unreads, live, latest) =
            row?;
        let live_mentions = serde_json::from_str(&live).map_err(|error| {
            StoreError::corrupt_item("conversation_read_state", &channel_id, error)
        })?;
        entries.push((
            channel_id,
            ConversationReadState {
                last_read,
                server_latest,
                server_mention_count: u64::try_from(mentions).unwrap_or_default(),
                server_unread_count: u64::try_from(unreads).unwrap_or_default(),
                server_has_unreads: has_unreads,
                live_mentions,
                live_latest_unread: latest,
            },
        ));
    }
    Ok(UnreadLedger::from_entries(entries))
}

fn upsert_sqlite_read_states(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    states: &[(String, ConversationReadState)],
) -> Result<bool> {
    if states.is_empty() {
        return Ok(false);
    }
    let mut changed = transaction.execute(
        "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(workspace_key) DO NOTHING",
        params![workspace_key, workspace_id],
    )? > 0;
    for (channel_id, state) in states {
        require_store_key("conversation read state", channel_id)?;
        let live_mentions = serde_json::to_string(&state.live_mentions)
            .context("failed to serialize live mentions")?;
        changed |= transaction.execute(
            "INSERT INTO conversation_read_state(
                 workspace_key, channel_id, last_read, server_latest, server_mention_count,
                 server_unread_count, server_has_unreads, live_mentions_json, live_latest_unread)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_key, channel_id) DO UPDATE SET
                 last_read = excluded.last_read,
                 server_latest = excluded.server_latest,
                 server_mention_count = excluded.server_mention_count,
                 server_unread_count = excluded.server_unread_count,
                 server_has_unreads = excluded.server_has_unreads,
                 live_mentions_json = excluded.live_mentions_json,
                 live_latest_unread = excluded.live_latest_unread",
            params![
                workspace_key,
                channel_id,
                state.last_read,
                state.server_latest,
                i64::try_from(state.server_mention_count).unwrap_or(i64::MAX),
                i64::try_from(state.server_unread_count).unwrap_or(i64::MAX),
                state.server_has_unreads,
                live_mentions,
                state.live_latest_unread,
            ],
        )? > 0;
    }
    Ok(changed)
}

fn load_sqlite_kind_map<T: DeserializeOwned>(
    connection: &Connection,
    workspace_key: &str,
    kind: &str,
) -> Result<HashMap<String, T>> {
    let mut statement = connection.prepare(
        "SELECT item_key, payload_json FROM workspace_items
         WHERE workspace_key = ?1 AND kind = ?2 ORDER BY item_key",
    )?;
    let rows = statement.query_map(params![workspace_key, kind], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut values = HashMap::new();
    for row in rows {
        let (key, payload) = row?;
        let value = serde_json::from_str(&payload)
            .map_err(|error| StoreError::corrupt_item(kind, &key, error))?;
        values.insert(key, value);
    }
    Ok(values)
}

fn load_sqlite_kind_values<T: DeserializeOwned>(
    connection: &Connection,
    workspace_key: &str,
    kind: &str,
) -> Result<Vec<T>> {
    Ok(load_sqlite_kind_map(connection, workspace_key, kind)?
        .into_values()
        .collect())
}

fn load_sqlite_item<T: DeserializeOwned>(
    connection: &Connection,
    workspace_key: &str,
    kind: &str,
    item_key: &str,
) -> Result<Option<T>> {
    let payload = connection
        .query_row(
            "SELECT payload_json FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
            params![workspace_key, kind, item_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    payload
        .map(|payload| {
            serde_json::from_str(&payload)
                .map_err(|error| StoreError::corrupt_item(kind, item_key, error))
        })
        .transpose()
}

fn load_sqlite_conversation(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
) -> Result<Option<SlackConversation>> {
    let payload = transaction
        .query_row(
            "SELECT payload_json
             FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
            params![workspace_key, channel_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let conversation = payload
        .map(|payload| {
            serde_json::from_str::<SlackConversation>(&payload)
                .map_err(|error| StoreError::corrupt_item("conversation", channel_id, error))
        })
        .transpose()?;
    Ok(conversation.filter(|conversation| conversation.id == channel_id))
}

fn upsert_sqlite_conversation(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    conversation: &SlackConversation,
) -> Result<bool> {
    let mut changed = transaction.execute(
        "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(workspace_key) DO UPDATE SET workspace_id = excluded.workspace_id
         WHERE workspaces.workspace_id IS NOT excluded.workspace_id",
        params![workspace_key, workspace_id],
    )? > 0;
    let conversation = conversation_for_cache(conversation);
    let payload = serde_json::to_string(&conversation)
        .context("failed to serialize cached workspace item")?;
    changed |= transaction.execute(
        "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
         VALUES (?1, 'conversation', ?2, ?3)
         ON CONFLICT(workspace_key, kind, item_key)
         DO UPDATE SET payload_json = excluded.payload_json
         WHERE workspace_items.payload_json IS NOT excluded.payload_json",
        params![workspace_key, conversation.id, payload],
    )? > 0;
    Ok(changed)
}

fn load_sqlite_search_state(
    connection: &Connection,
    workspace_key: &str,
) -> Result<Option<SearchProviderState>> {
    let workspace_id = connection
        .query_row(
            "SELECT workspace_id FROM workspaces WHERE workspace_key = ?1",
            [workspace_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };

    let mut state = SearchProviderState {
        workspace_id,
        conversations: Vec::new(),
        user_names: HashMap::new(),
        user_full_names: HashMap::new(),
        user_search_aliases: HashMap::new(),
    };
    let mut statement = connection.prepare(
        "SELECT kind, item_key, payload_json
         FROM workspace_items
         WHERE workspace_key = ?1
           AND kind IN ('conversation', 'user_name', 'user_full_name', 'user_aliases')
         ORDER BY kind, item_key",
    )?;
    let rows = statement.query_map([workspace_key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, item_key, payload) = row?;
        match kind.as_str() {
            "conversation" => {
                let item = serde_json::from_str(&payload)
                    .map_err(|error| StoreError::corrupt_item("conversation", &item_key, error))?;
                state.conversations.push(item);
            }
            "user_name" => {
                let item = serde_json::from_str(&payload)
                    .map_err(|error| StoreError::corrupt_item("user_name", &item_key, error))?;
                state.user_names.insert(item_key, item);
            }
            "user_full_name" => {
                let item = serde_json::from_str(&payload).map_err(|error| {
                    StoreError::corrupt_item("user_full_name", &item_key, error)
                })?;
                state.user_full_names.insert(item_key, item);
            }
            "user_aliases" => {
                let item = serde_json::from_str(&payload)
                    .map_err(|error| StoreError::corrupt_item("user_aliases", &item_key, error))?;
                state.user_search_aliases.insert(item_key, item);
            }
            _ => unreachable!("search-state query returned an unexpected item kind"),
        }
    }
    Ok(Some(state))
}

fn import_legacy_state(
    connection: &mut Connection,
    workspace_key: &str,
    state: &LegacyWorkspaceState,
    activate: bool,
) -> Result<()> {
    let desired = legacy_state_items(state)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_sqlite_workspace(&transaction, workspace_key, &state.workspace_id, activate)?;
    sync_state_items(&transaction, workspace_key, desired)?;
    transaction.commit()?;
    Ok(())
}

fn apply_store_change(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    change: StoreChange,
) -> Result<bool> {
    match change {
        StoreChange::BootstrapReplaced(data) => {
            let mut changed = sync_conversations(transaction, workspace_key, data.conversations)?;
            changed |= sync_users(transaction, workspace_key, data.users)?;
            changed |= sync_sqlite_kind(
                transaction,
                workspace_key,
                "channel_history",
                data.histories
                    .into_iter()
                    .map(|(channel_id, messages)| {
                        require_store_key("channel history", &channel_id)?;
                        Ok((
                            channel_id,
                            channel_timeline_messages(persistable_messages(messages)),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )?;
            changed |= sync_thread_records(transaction, workspace_key, data.threads)?;
            changed |= transaction.execute(
                "DELETE FROM conversation_read_state WHERE workspace_key = ?1",
                [workspace_key],
            )? > 0;
            changed |= upsert_sqlite_read_states(
                transaction,
                workspace_key,
                workspace_id,
                &data.read_states.entries(),
            )?;
            Ok(changed)
        }
        StoreChange::ConversationsReplaced(conversations) => {
            sync_conversations(transaction, workspace_key, conversations)
        }
        StoreChange::ConversationsRepaired(conversations) => {
            sync_repaired_conversations(transaction, workspace_key, workspace_id, conversations)
        }
        StoreChange::ConversationUpsert(conversation) => {
            require_store_key("conversation", &conversation.id)?;
            upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation)
        }
        StoreChange::ConversationMetadataUpsert(conversation) => {
            require_store_key("conversation", &conversation.id)?;
            upsert_sqlite_conversation_metadata(
                transaction,
                workspace_key,
                workspace_id,
                conversation,
                true,
                false,
            )
        }
        StoreChange::ConversationMembershipUpsert(conversation) => {
            require_store_key("conversation", &conversation.id)?;
            upsert_sqlite_conversation_metadata(
                transaction,
                workspace_key,
                workspace_id,
                conversation,
                false,
                true,
            )
        }
        StoreChange::ConversationStarChanged {
            channel_id,
            starred,
        } => {
            require_store_key("conversation star", &channel_id)?;
            let mut conversation =
                load_sqlite_conversation(transaction, workspace_key, &channel_id)?.unwrap_or_else(
                    || SlackConversation {
                        id: channel_id,
                        ..Default::default()
                    },
                );
            conversation.is_starred = Some(starred);
            upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation)
        }
        StoreChange::ConversationRemoved { channel_id } => {
            require_store_key("conversation", &channel_id)?;
            Ok(transaction.execute(
                "DELETE FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                params![workspace_key, channel_id],
            )? > 0)
        }
        StoreChange::UsersReplaced(users) => sync_users(transaction, workspace_key, users),
        StoreChange::UserUpsert(user) => upsert_user_projection(transaction, workspace_key, user),
        StoreChange::MessageDelta {
            channel_id,
            message,
            kind,
        } => {
            require_store_key("message channel", &channel_id)?;
            require_store_key("message timestamp", &message.ts)?;
            if is_ephemeral(&message) {
                return Ok(false);
            }
            apply_message_delta(transaction, workspace_key, &channel_id, message, kind)
        }
        StoreChange::HistoryReplaced {
            channel_id,
            messages,
        } => {
            require_store_key("channel history", &channel_id)?;
            upsert_sqlite_item(
                transaction,
                workspace_key,
                "channel_history",
                &channel_id,
                &channel_timeline_messages(persistable_messages(messages)),
            )
        }
        StoreChange::HistoryDelta {
            channel_id,
            messages,
        } => {
            require_store_key("channel history", &channel_id)?;
            let messages = persistable_messages(messages);
            let existing = normalize_cached_messages(
                load_sqlite_item::<Vec<SlackMessage>>(
                    transaction,
                    workspace_key,
                    "channel_history",
                    &channel_id,
                )?
                .unwrap_or_default(),
            );
            let merged = merge_channel_history_pages(&existing, &messages);
            upsert_sqlite_item(
                transaction,
                workspace_key,
                "channel_history",
                &channel_id,
                &merged,
            )
        }
        StoreChange::HistoryRemoved { channel_id } => {
            require_store_key("channel history", &channel_id)?;
            Ok(transaction.execute(
                "DELETE FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'channel_history' AND item_key = ?2",
                params![workspace_key, channel_id],
            )? > 0)
        }
        StoreChange::ThreadReplaced {
            channel_id,
            thread_ts,
            messages,
        } => {
            require_store_key("thread channel", &channel_id)?;
            require_store_key("thread timestamp", &thread_ts)?;
            upsert_sqlite_item(
                transaction,
                workspace_key,
                "thread_replies",
                &thread_key(&channel_id, &thread_ts),
                &pruned_history(persistable_messages(messages)),
            )
        }
        StoreChange::ThreadDelta {
            channel_id,
            thread_ts,
            messages,
        } => {
            require_store_key("thread channel", &channel_id)?;
            require_store_key("thread timestamp", &thread_ts)?;
            let messages = persistable_messages(messages);
            let existing = normalize_cached_messages(
                load_sqlite_item::<Vec<SlackMessage>>(
                    transaction,
                    workspace_key,
                    "thread_replies",
                    &thread_key(&channel_id, &thread_ts),
                )?
                .unwrap_or_default(),
            );
            let merged = merge_thread_replies_pages(&existing, &messages);
            upsert_sqlite_item(
                transaction,
                workspace_key,
                "thread_replies",
                &thread_key(&channel_id, &thread_ts),
                &merged,
            )
        }
        StoreChange::ThreadCatalogReplaced(records) => {
            sync_thread_records(transaction, workspace_key, records)
        }
        StoreChange::ThreadRecordsUpserted(records) => {
            upsert_thread_records(transaction, workspace_key, records)
        }
        StoreChange::ReadStatesUpserted(states) => {
            upsert_sqlite_read_states(transaction, workspace_key, workspace_id, &states)
        }
    }
}

#[derive(Clone)]
struct StoredThreadTimeline {
    item_key: String,
    root_ts: String,
    messages: Vec<SlackMessage>,
    original: Vec<SlackMessage>,
    existed: bool,
}

/// Ephemeral messages ("Only visible to you", such as a /giphy preview) are
/// never stored: Slack never returns them from history, so a stored copy
/// would come back after a restart as a ghost with dead buttons.
fn is_ephemeral(message: &SlackMessage) -> bool {
    message.is_ephemeral == Some(true)
}

fn persistable_messages(messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    normalize_cached_messages(messages)
        .into_iter()
        .filter(|message| !is_ephemeral(message))
        .collect()
}

fn apply_message_delta(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
    message: SlackMessage,
    kind: MessageMutationKind,
) -> Result<bool> {
    let mut message = normalize_cached_messages(vec![message])
        .pop()
        .expect("one normalized message");
    let existing_history = load_sqlite_item::<Vec<SlackMessage>>(
        transaction,
        workspace_key,
        "channel_history",
        channel_id,
    )?;
    let original_history = existing_history
        .clone()
        .map(normalize_cached_messages)
        .map(channel_timeline_messages)
        .unwrap_or_default();
    let mut history = original_history.clone();
    let mut threads = if kind == MessageMutationKind::Posted {
        load_sqlite_posted_message_thread(transaction, workspace_key, channel_id, &message)?
    } else {
        load_sqlite_channel_threads(transaction, workspace_key, channel_id)?
    };

    let previous_identity_messages = history
        .iter()
        .chain(threads.iter().flat_map(|thread| thread.messages.iter()))
        .filter(|known| same_message_identity(known, &message))
        .cloned()
        .collect::<Vec<_>>();
    let previous_identity_found = !previous_identity_messages.is_empty();
    if kind == MessageMutationKind::Changed && message.thread_root_ts().is_none() {
        if let Some(previous_root) = canonical_root_message(&history, &threads, &message.ts) {
            preserve_missing_store_root_aggregates(&mut message, &previous_root);
        }
    }
    let mut previous_replies = BTreeMap::<String, Vec<SlackMessage>>::new();
    for previous in &previous_identity_messages {
        if let Some(root_ts) = previous.thread_root_ts() {
            previous_replies
                .entry(root_ts.to_string())
                .or_default()
                .push(previous.clone());
        }
    }

    let next_root = if kind == MessageMutationKind::Deleted {
        None
    } else {
        message.thread_root_ts().map(str::to_string)
    };
    let mut affected_roots = previous_replies.keys().cloned().collect::<BTreeSet<_>>();
    affected_roots.extend(next_root.iter().cloned());
    let roots_before = affected_roots
        .iter()
        .filter_map(|root_ts| {
            canonical_root_message(&history, &threads, root_ts).map(|root| (root_ts.clone(), root))
        })
        .collect::<BTreeMap<_, _>>();

    history.retain(|known| !same_message_identity(known, &message));
    for thread in &mut threads {
        thread
            .messages
            .retain(|known| !same_message_identity(known, &message));
    }

    if kind != MessageMutationKind::Deleted {
        if message.belongs_in_channel_timeline() {
            history.push(message.clone());
        }
        if let Some(root_ts) = next_root.as_deref() {
            let thread = ensure_stored_thread(&mut threads, channel_id, root_ts);
            thread.messages.push(message.clone());
        } else {
            let own_thread_exists = threads.iter().any(|thread| thread.root_ts == message.ts);
            let has_thread_root_aggregate = message.reply_count.is_some()
                || message.latest_reply.is_some()
                || message.reply_users.is_some();
            if own_thread_exists
                || message.thread_ts.as_deref() == Some(message.ts.as_str())
                || has_thread_root_aggregate
            {
                let thread = ensure_stored_thread(&mut threads, channel_id, &message.ts);
                thread.messages.push(message.clone());
            }
        }
    }

    for root_ts in affected_roots {
        let Some(mut root) = canonical_root_message(&history, &threads, &root_ts)
            .or_else(|| roots_before.get(&root_ts).cloned())
        else {
            continue;
        };
        if let Some(before) = roots_before.get(&root_ts) {
            merge_root_aggregates(&mut root, before);
        }

        let previous = previous_replies.get(&root_ts);
        let old_present = previous.is_some_and(|messages| !messages.is_empty());
        let new_present = next_root.as_deref() == Some(root_ts.as_str());
        match (old_present, new_present) {
            (true, false) => {
                root.reply_count = Some(root.reply_count.unwrap_or_default().saturating_sub(1));
            }
            (false, true) => {
                let addition_is_proven = match kind {
                    MessageMutationKind::Posted => !previous_identity_found,
                    MessageMutationKind::Changed => previous_identity_found,
                    MessageMutationKind::Deleted => false,
                };
                if addition_is_proven {
                    root.reply_count = Some(root.reply_count.unwrap_or_default().saturating_add(1));
                }
            }
            (true, true) | (false, false) => {}
        }

        let removed_timestamps = previous
            .into_iter()
            .flatten()
            .map(|previous| previous.ts.as_str())
            .collect::<BTreeSet<_>>();
        let removed_latest = root
            .latest_reply
            .as_deref()
            .is_some_and(|latest| removed_timestamps.contains(latest))
            && (!new_present || root.latest_reply.as_deref() != Some(message.ts.as_str()));
        if removed_latest {
            root.latest_reply = latest_cached_reply(&threads, &root_ts);
        }
        if new_present
            && root
                .latest_reply
                .as_deref()
                .is_none_or(|latest| slack_timestamp_is_after(&message.ts, latest))
        {
            root.latest_reply = Some(message.ts.clone());
        }

        if root.reply_count == Some(0) {
            root.latest_reply = None;
            root.reply_users = Some(Vec::new());
        } else if let Some(reply_users) =
            complete_cached_reply_users(&threads, &root_ts, root.reply_count)
        {
            root.reply_users = Some(reply_users);
        } else if new_present {
            if let Some(user_id) = message.user.as_deref() {
                let users = root.reply_users.get_or_insert_with(Vec::new);
                if !users.iter().any(|known| known == user_id) {
                    users.push(user_id.to_string());
                }
            }
        }
        replace_root_aggregates(&mut history, &mut threads, &root_ts, &root);
    }

    history = channel_timeline_messages(history);
    for thread in &mut threads {
        thread.messages = pruned_history(std::mem::take(&mut thread.messages));
    }
    threads.sort_by(|left, right| left.item_key.cmp(&right.item_key));

    let mut changed = false;
    if history != original_history || existing_history.is_some_and(|_| history.is_empty()) {
        changed |= replace_timeline_item(
            transaction,
            workspace_key,
            "channel_history",
            channel_id,
            history,
        )?;
    }
    for thread in threads {
        if thread.messages != thread.original || (thread.existed && thread.messages.is_empty()) {
            changed |= replace_timeline_item(
                transaction,
                workspace_key,
                "thread_replies",
                &thread.item_key,
                thread.messages,
            )?;
        }
    }
    Ok(changed)
}

fn load_sqlite_channel_threads(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
) -> Result<Vec<StoredThreadTimeline>> {
    let key_prefix = format!("{channel_id}:");
    let key_upper_bound = format!("{channel_id};");
    let mut statement = transaction.prepare(
        "SELECT item_key, payload_json
         FROM workspace_items
         WHERE workspace_key = ?1
           AND kind = 'thread_replies'
           AND item_key >= ?2
           AND item_key < ?3
         ORDER BY item_key",
    )?;
    let rows = statement.query_map(params![workspace_key, key_prefix, key_upper_bound], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut threads = Vec::new();
    for row in rows {
        let (item_key, payload) = row?;
        let Some(root_ts) = item_key.strip_prefix(&key_prefix) else {
            continue;
        };
        if root_ts.trim().is_empty() {
            continue;
        }
        let root_ts = root_ts.to_string();
        let messages = serde_json::from_str::<Vec<SlackMessage>>(&payload)
            .map_err(|error| StoreError::corrupt_item("thread_replies", &item_key, error))?;
        let messages = pruned_history(normalize_cached_messages(messages));
        threads.push(StoredThreadTimeline {
            item_key,
            root_ts,
            original: messages.clone(),
            messages,
            existed: true,
        });
    }
    Ok(threads)
}

fn load_sqlite_posted_message_thread(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
    message: &SlackMessage,
) -> Result<Vec<StoredThreadTimeline>> {
    let root_ts = message.thread_root_ts().or_else(|| {
        (message.thread_ts.as_deref() == Some(message.ts.as_str())
            || message.reply_count.is_some()
            || message.latest_reply.is_some()
            || message.reply_users.is_some())
        .then_some(message.ts.as_str())
    });
    let Some(root_ts) = root_ts else {
        return Ok(Vec::new());
    };
    let item_key = thread_key(channel_id, root_ts);
    let Some(messages) = load_sqlite_item::<Vec<SlackMessage>>(
        transaction,
        workspace_key,
        "thread_replies",
        &item_key,
    )?
    else {
        return Ok(Vec::new());
    };
    let messages = pruned_history(normalize_cached_messages(messages));
    Ok(vec![StoredThreadTimeline {
        item_key,
        root_ts: root_ts.to_string(),
        original: messages.clone(),
        messages,
        existed: true,
    }])
}

fn ensure_stored_thread<'a>(
    threads: &'a mut Vec<StoredThreadTimeline>,
    channel_id: &str,
    root_ts: &str,
) -> &'a mut StoredThreadTimeline {
    if let Some(index) = threads.iter().position(|thread| thread.root_ts == root_ts) {
        return &mut threads[index];
    }
    threads.push(StoredThreadTimeline {
        item_key: thread_key(channel_id, root_ts),
        root_ts: root_ts.to_string(),
        messages: Vec::new(),
        original: Vec::new(),
        existed: false,
    });
    threads
        .last_mut()
        .expect("a newly inserted thread timeline must exist")
}

fn canonical_root_message(
    history: &[SlackMessage],
    threads: &[StoredThreadTimeline],
    root_ts: &str,
) -> Option<SlackMessage> {
    let mut candidates = history
        .iter()
        .filter(|message| message.ts == root_ts)
        .chain(
            threads
                .iter()
                .filter(|thread| thread.root_ts == root_ts)
                .flat_map(|thread| thread.messages.iter())
                .filter(|message| message.ts == root_ts),
        );
    let mut root = candidates.next()?.clone();
    for candidate in candidates {
        merge_root_aggregates(&mut root, candidate);
    }
    Some(root)
}

fn merge_root_aggregates(root: &mut SlackMessage, candidate: &SlackMessage) {
    if candidate.reply_count > root.reply_count {
        root.reply_count = candidate.reply_count;
    }
    if candidate
        .latest_reply
        .as_deref()
        .is_some_and(|candidate_ts| {
            root.latest_reply
                .as_deref()
                .is_none_or(|current_ts| slack_timestamp_is_after(candidate_ts, current_ts))
        })
    {
        root.latest_reply.clone_from(&candidate.latest_reply);
    }
    if let Some(candidate_users) = candidate.reply_users.as_ref() {
        let users = root.reply_users.get_or_insert_with(Vec::new);
        for user_id in candidate_users {
            if !users.iter().any(|known| known == user_id) {
                users.push(user_id.clone());
            }
        }
    }
}

fn preserve_missing_store_root_aggregates(message: &mut SlackMessage, previous: &SlackMessage) {
    if message.reply_count.is_none() {
        message.reply_count = previous.reply_count;
    }
    if message.latest_reply.is_none() {
        message.latest_reply.clone_from(&previous.latest_reply);
    }
    if message.reply_users.is_none() {
        message.reply_users.clone_from(&previous.reply_users);
    }
}

fn latest_cached_reply(threads: &[StoredThreadTimeline], root_ts: &str) -> Option<String> {
    threads
        .iter()
        .filter(|thread| thread.root_ts == root_ts)
        .flat_map(|thread| thread.messages.iter())
        .filter(|message| message.thread_root_ts() == Some(root_ts))
        .max_by(|left, right| left.ts.cmp(&right.ts))
        .map(|message| message.ts.clone())
}

fn complete_cached_reply_users(
    threads: &[StoredThreadTimeline],
    root_ts: &str,
    authoritative_reply_count: Option<u64>,
) -> Option<Vec<String>> {
    let replies = threads
        .iter()
        .filter(|thread| thread.root_ts == root_ts)
        .flat_map(|thread| thread.messages.iter())
        .filter(|message| message.thread_root_ts() == Some(root_ts))
        .collect::<Vec<_>>();
    if authoritative_reply_count != Some(replies.len() as u64) {
        return None;
    }
    let mut users = Vec::new();
    for user_id in replies.iter().filter_map(|message| message.user.as_ref()) {
        if !users.iter().any(|known| known == user_id) {
            users.push(user_id.clone());
        }
    }
    Some(users)
}

fn replace_root_aggregates(
    history: &mut [SlackMessage],
    threads: &mut [StoredThreadTimeline],
    root_ts: &str,
    root: &SlackMessage,
) {
    for message in history.iter_mut().filter(|message| message.ts == root_ts) {
        message.reply_count = root.reply_count;
        message.latest_reply.clone_from(&root.latest_reply);
        message.reply_users.clone_from(&root.reply_users);
    }
    for message in threads
        .iter_mut()
        .filter(|thread| thread.root_ts == root_ts)
        .flat_map(|thread| thread.messages.iter_mut())
        .filter(|message| message.ts == root_ts)
    {
        message.reply_count = root.reply_count;
        message.latest_reply.clone_from(&root.latest_reply);
        message.reply_users.clone_from(&root.reply_users);
    }
}

fn replace_timeline_item(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
    item_key: &str,
    messages: Vec<SlackMessage>,
) -> Result<bool> {
    if messages.is_empty() {
        return Ok(transaction.execute(
            "DELETE FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
            params![workspace_key, kind, item_key],
        )? > 0);
    }
    upsert_sqlite_item(transaction, workspace_key, kind, item_key, &messages)
}

fn sync_conversations(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    conversations: impl IntoIterator<Item = SlackConversation>,
) -> Result<bool> {
    let conversations = conversations
        .into_iter()
        .map(|conversation| {
            require_store_key("conversation", &conversation.id)?;
            let conversation = conversation_for_cache(&conversation);
            Ok((conversation.id.clone(), conversation))
        })
        .collect::<Result<Vec<_>>>()?;
    sync_sqlite_kind(transaction, workspace_key, "conversation", conversations)
}

fn upsert_sqlite_conversation_metadata(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    conversation: SlackConversation,
    preserve_existing_star: bool,
    insert_full_if_missing: bool,
) -> Result<bool> {
    let existing = load_sqlite_conversation(transaction, workspace_key, &conversation.id)?;
    if existing.is_none() && insert_full_if_missing {
        return upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation);
    }
    let existing_star = if preserve_existing_star {
        existing
            .as_ref()
            .and_then(|conversation| conversation.is_starred)
    } else {
        None
    };
    let mut catalog = ConversationCatalog::from_cached(existing);
    catalog.upsert_metadata(conversation);
    let mut conversation = catalog
        .conversations()
        .into_iter()
        .next()
        .expect("metadata upsert should produce one conversation");
    if let Some(existing_star) = existing_star {
        conversation.is_starred = Some(existing_star);
    }
    upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation)
}

fn sync_repaired_conversations(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    conversations: Vec<SlackConversation>,
) -> Result<bool> {
    let mut desired = HashSet::new();
    let mut changed = false;
    for conversation in conversations {
        require_store_key("conversation", &conversation.id)?;
        desired.insert(conversation.id.clone());
        changed |= upsert_sqlite_conversation_metadata(
            transaction,
            workspace_key,
            workspace_id,
            conversation,
            false,
            true,
        )?;
    }

    let existing = {
        let mut statement = transaction.prepare(
            "SELECT item_key FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'conversation'",
        )?;
        let existing = statement
            .query_map([workspace_key], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        existing
    };
    for channel_id in existing {
        if !desired.contains(&channel_id) {
            changed |= transaction.execute(
                "DELETE FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                params![workspace_key, channel_id],
            )? > 0;
        }
    }
    Ok(changed)
}

fn sync_users(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    users: impl IntoIterator<Item = SlackUser>,
) -> Result<bool> {
    let mut projections = CachedUserProjections::default();
    for user in users {
        projections.insert(user)?;
    }
    let mut changed = sync_sqlite_kind(
        transaction,
        workspace_key,
        "user_name",
        projections.display_names,
    )?;
    changed |= sync_sqlite_kind(
        transaction,
        workspace_key,
        "user_full_name",
        projections.full_names,
    )?;
    changed |= sync_sqlite_kind(
        transaction,
        workspace_key,
        "user_avatar_url",
        projections.avatar_urls,
    )?;
    changed |= sync_sqlite_kind(
        transaction,
        workspace_key,
        "user_aliases",
        projections.aliases,
    )?;
    changed |= sync_sqlite_kind(
        transaction,
        workspace_key,
        "user_status",
        projections.statuses,
    )?;
    Ok(changed)
}

#[derive(Default)]
struct CachedUserProjections {
    display_names: HashMap<String, String>,
    full_names: HashMap<String, String>,
    avatar_urls: HashMap<String, String>,
    aliases: HashMap<String, Vec<String>>,
    statuses: HashMap<String, SlackUserStatus>,
}

impl CachedUserProjections {
    fn insert(&mut self, user: SlackUser) -> Result<()> {
        let user_id = user_projection_id(&user)?;
        if let Some(display_name) = user.display_name() {
            self.display_names.insert(user_id.clone(), display_name);
        }
        if let Some(full_name) = user.full_name() {
            self.full_names.insert(user_id.clone(), full_name);
        }
        if let Some(avatar_url) = user.avatar_url() {
            self.avatar_urls.insert(user_id.clone(), avatar_url);
        }
        let aliases = user.search_aliases();
        if !aliases.is_empty() {
            self.aliases.insert(user_id.clone(), aliases);
        }
        if let Some(status) = user.status() {
            self.statuses.insert(user_id, status);
        }
        Ok(())
    }
}

fn upsert_user_projection(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    user: SlackUser,
) -> Result<bool> {
    let user_id = user_projection_id(&user)?;
    let clears_status = user
        .profile
        .as_ref()
        .is_some_and(|profile| profile.contains_status_fields() && profile.status().is_none());
    let mut projections = CachedUserProjections::default();
    projections.insert(user)?;
    let mut changed = false;
    for (user_id, display_name) in projections.display_names {
        changed |= upsert_sqlite_item(
            transaction,
            workspace_key,
            "user_name",
            &user_id,
            &display_name,
        )?;
    }
    for (user_id, full_name) in projections.full_names {
        changed |= upsert_sqlite_item(
            transaction,
            workspace_key,
            "user_full_name",
            &user_id,
            &full_name,
        )?;
    }
    for (user_id, avatar_url) in projections.avatar_urls {
        changed |= upsert_sqlite_item(
            transaction,
            workspace_key,
            "user_avatar_url",
            &user_id,
            &avatar_url,
        )?;
    }
    for (user_id, aliases) in projections.aliases {
        changed |= upsert_sqlite_item(
            transaction,
            workspace_key,
            "user_aliases",
            &user_id,
            &aliases,
        )?;
    }
    for (user_id, status) in projections.statuses {
        changed |=
            upsert_sqlite_item(transaction, workspace_key, "user_status", &user_id, &status)?;
    }
    if clears_status {
        changed |= transaction.execute(
            "DELETE FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'user_status' AND item_key = ?2",
            params![workspace_key, user_id],
        )? > 0;
    }
    Ok(changed)
}

fn user_projection_id(user: &SlackUser) -> Result<String> {
    user.id
        .as_deref()
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| StoreError::rejected_update("user id must not be empty"))
}

fn sync_thread_records(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    records: Vec<ThreadRecord>,
) -> Result<bool> {
    let records = records
        .into_iter()
        .map(|record| {
            require_store_key("thread channel", &record.key.channel_id)?;
            require_store_key("thread timestamp", &record.key.root_ts)?;
            Ok((
                thread_key(&record.key.channel_id, &record.key.root_ts),
                record,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    sync_sqlite_kind(transaction, workspace_key, "thread_record", records)
}

fn upsert_thread_records(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    records: Vec<ThreadRecord>,
) -> Result<bool> {
    if records.is_empty() {
        return Ok(false);
    }
    let mut changed = false;
    for record in records {
        require_store_key("thread channel", &record.key.channel_id)?;
        require_store_key("thread timestamp", &record.key.root_ts)?;
        let item_key = thread_key(&record.key.channel_id, &record.key.root_ts);
        let payload =
            serde_json::to_string(&record).context("failed to serialize cached thread record")?;
        changed |= transaction.execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, 'thread_record', ?2, ?3)
             ON CONFLICT(workspace_key, kind, item_key) DO UPDATE SET payload_json = excluded.payload_json",
            params![workspace_key, item_key, payload],
        )? > 0;
    }
    Ok(changed)
}

fn require_store_key(kind: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(StoreError::rejected_update(format!(
            "{kind} key must not be empty"
        )));
    }
    Ok(())
}

fn ensure_sqlite_workspace(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    activate: bool,
) -> Result<bool> {
    let mut changed = transaction.execute(
        "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(workspace_key) DO UPDATE SET workspace_id = excluded.workspace_id
         WHERE workspaces.workspace_id IS NOT excluded.workspace_id",
        params![workspace_key, workspace_id],
    )? > 0;
    if activate {
        changed |= transaction.execute(
            "UPDATE app_state SET active_workspace_key = ?1
             WHERE singleton = 1 AND active_workspace_key IS NOT ?1",
            [workspace_key],
        )? > 0;
    }
    Ok(changed)
}

fn finish_sqlite_transaction(transaction: Transaction<'_>, changed: bool) -> Result<bool> {
    if !changed {
        transaction.rollback()?;
        Ok(false)
    } else {
        transaction.commit()?;
        Ok(true)
    }
}

fn reset_sqlite_workspace(connection: &mut Connection, workspace_key: &str) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "UPDATE app_state SET active_workspace_key = NULL
         WHERE singleton = 1 AND active_workspace_key = ?1",
        [workspace_key],
    )?;
    transaction.execute(
        "DELETE FROM workspaces WHERE workspace_key = ?1",
        [workspace_key],
    )?;
    transaction.commit()?;
    Ok(())
}

fn delete_sqlite_kind(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
) -> Result<bool> {
    Ok(transaction.execute(
        "DELETE FROM workspace_items WHERE workspace_key = ?1 AND kind = ?2",
        params![workspace_key, kind],
    )? > 0)
}

fn delete_sqlite_workspace_item(
    connection: &mut Connection,
    workspace_key: &str,
    kind: &str,
    item_key: &str,
) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "DELETE FROM workspace_items
         WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
        params![workspace_key, kind, item_key],
    )?;
    transaction.commit()?;
    Ok(())
}

fn sync_state_items(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    desired: HashMap<(String, String), String>,
) -> Result<()> {
    let mut current = HashMap::new();
    {
        let mut statement = transaction.prepare(
            "SELECT kind, item_key, payload_json FROM workspace_items WHERE workspace_key = ?1",
        )?;
        let rows = statement.query_map([workspace_key], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (key, payload) = row?;
            current.insert(key, payload);
        }
    }

    for ((kind, item_key), _) in current
        .iter()
        .filter(|(key, _)| !desired.contains_key(*key))
    {
        transaction.execute(
            "DELETE FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
            params![workspace_key, kind, item_key],
        )?;
    }
    for ((kind, item_key), payload) in desired {
        if current.get(&(kind.clone(), item_key.clone())) == Some(&payload) {
            continue;
        }
        transaction.execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_key, kind, item_key)
             DO UPDATE SET payload_json = excluded.payload_json",
            params![workspace_key, kind, item_key, payload],
        )?;
    }
    Ok(())
}

fn upsert_sqlite_item<T: Serialize>(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
    item_key: &str,
    value: &T,
) -> Result<bool> {
    let payload = serde_json::to_string(value).context("failed to serialize cached item")?;
    let changed = transaction.execute(
        "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_key, kind, item_key)
         DO UPDATE SET payload_json = excluded.payload_json
         WHERE workspace_items.payload_json IS NOT excluded.payload_json",
        params![workspace_key, kind, item_key, payload],
    )? > 0;
    Ok(changed)
}

fn sync_sqlite_kind<T: Serialize>(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
    values: impl IntoIterator<Item = (String, T)>,
) -> Result<bool> {
    let desired = values
        .into_iter()
        .map(|(key, value)| {
            Ok((
                key,
                serde_json::to_string(&value).context("failed to serialize cached item")?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut current = HashMap::new();
    {
        let mut statement = transaction.prepare(
            "SELECT item_key, payload_json FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2",
        )?;
        let rows = statement.query_map(params![workspace_key, kind], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (key, payload) = row?;
            current.insert(key, payload);
        }
    }
    let mut changed = false;
    for key in current.keys().filter(|key| !desired.contains_key(*key)) {
        changed |= transaction.execute(
            "DELETE FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
            params![workspace_key, kind, key],
        )? > 0;
    }
    for (key, payload) in desired {
        if current.get(&key) == Some(&payload) {
            continue;
        }
        changed |= transaction.execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_key, kind, item_key)
             DO UPDATE SET payload_json = excluded.payload_json",
            params![workspace_key, kind, key, payload],
        )? > 0;
    }
    Ok(changed)
}

fn legacy_state_items(state: &LegacyWorkspaceState) -> Result<HashMap<(String, String), String>> {
    let mut items = HashMap::new();
    for conversation in &state.conversations {
        let conversation = conversation_for_cache(conversation);
        insert_state_item(
            &mut items,
            "conversation",
            conversation.id.clone(),
            &conversation,
        )?;
    }
    for (key, value) in &state.user_names {
        insert_state_item(&mut items, "user_name", key.clone(), value)?;
    }
    for (key, value) in &state.user_full_names {
        insert_state_item(&mut items, "user_full_name", key.clone(), value)?;
    }
    for (key, value) in &state.user_avatar_urls {
        insert_state_item(&mut items, "user_avatar_url", key.clone(), value)?;
    }
    for (key, value) in &state.user_search_aliases {
        insert_state_item(&mut items, "user_aliases", key.clone(), value)?;
    }
    for (key, value) in &state.user_statuses {
        insert_state_item(&mut items, "user_status", key.clone(), value)?;
    }
    for (key, value) in &state.channel_histories {
        insert_state_item(
            &mut items,
            "channel_history",
            key.clone(),
            &normalize_cached_messages(value.clone()),
        )?;
    }
    for (key, value) in &state.thread_replies {
        insert_state_item(
            &mut items,
            "thread_replies",
            key.clone(),
            &normalize_cached_messages(value.clone()),
        )?;
    }
    for record in &state.thread_catalog {
        insert_state_item(
            &mut items,
            "thread_record",
            thread_key(&record.key.channel_id, &record.key.root_ts),
            record,
        )?;
    }
    for (key, value) in &state.custom_emojis {
        insert_state_item(&mut items, "custom_emoji", key.clone(), value)?;
    }
    if !state.attention_deliveries.is_empty() {
        insert_state_item(
            &mut items,
            ATTENTION_DELIVERY_KIND,
            ATTENTION_DELIVERY_LEDGER_KEY.to_string(),
            &state.attention_deliveries,
        )?;
    }
    Ok(items)
}

fn attention_delivery_identity(channel_id: &str, message_ts: &str) -> Option<String> {
    let channel_id = channel_id.trim();
    let message_ts = message_ts.trim();
    if channel_id.is_empty() || message_ts.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(channel_id.as_bytes());
    digest.update([0]);
    digest.update(message_ts.as_bytes());
    Some(
        digest
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                let _ = write!(output, "{byte:02x}");
                output
            }),
    )
}

fn conversation_for_cache(conversation: &SlackConversation) -> SlackConversation {
    let mut cached = conversation.clone();
    let remove_empty_properties = cached
        .extra
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .is_some_and(|properties| {
            properties.remove("huddles");
            properties.is_empty()
        });
    if remove_empty_properties {
        cached.extra.remove("properties");
    }
    // Read state is owned by `conversation_read_state`; drop payload copies
    // (including legacy cached fields) so they cannot go stale here.
    for key in LEGACY_CONVERSATION_READ_KEYS {
        cached.extra.remove(key);
    }
    cached
}

fn insert_state_item<T: Serialize + ?Sized>(
    items: &mut HashMap<(String, String), String>,
    kind: &str,
    key: String,
    value: &T,
) -> Result<()> {
    items.insert(
        (kind.to_string(), key),
        serde_json::to_string(value).context("failed to serialize cached workspace item")?,
    );
    Ok(())
}

fn migrate_legacy_workspace(
    connection: &mut Connection,
    directory: &Path,
    workspace_key: &str,
    workspace_id: &str,
) -> Result<()> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_key = ?1)",
        [workspace_key],
        |row| row.get::<_, bool>(0),
    )?;
    if exists {
        return Ok(());
    }
    let Some(mut state) = read_legacy_state(directory, workspace_key)? else {
        return Ok(());
    };
    state.workspace_id = workspace_id.to_string();
    import_legacy_state(connection, workspace_key, &state, false)?;
    remove_legacy_workspace_files(directory, workspace_key);
    Ok(())
}

fn legacy_active_workspace_candidate(
    directory: &Path,
) -> Result<Option<(String, LegacyWorkspaceState)>> {
    let marked = std::fs::read_to_string(directory.join("active-workspace"))
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| is_workspace_key(key))
        .and_then(|key| {
            read_legacy_state(directory, &key)
                .ok()
                .flatten()
                .filter(|state| !state.workspace_id.trim().is_empty())
                .map(|state| (key, state))
        });
    if let Some(marked) = marked {
        Ok(Some(marked))
    } else {
        let mut candidates = legacy_states(directory)?;
        Ok((candidates.len() == 1).then(|| candidates.remove(0)))
    }
}

fn migrate_legacy_active_workspace(connection: &mut Connection, directory: &Path) -> Result<()> {
    let active = connection
        .query_row(
            "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if active.is_some() {
        return Ok(());
    }

    if let Some((workspace_key, state)) = legacy_active_workspace_candidate(directory)? {
        import_legacy_state(connection, &workspace_key, &state, true)?;
        remove_legacy_workspace_files(directory, &workspace_key);
        let _ = std::fs::remove_file(directory.join("active-workspace"));
    }
    Ok(())
}

fn remove_legacy_workspace_files(directory: &Path, workspace_key: &str) {
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.json")));
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.search.json")));
}

fn legacy_states(directory: &Path) -> Result<Vec<(String, LegacyWorkspaceState)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut states = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        let Some(key) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_workspace_key(key) {
            continue;
        }
        if let Some(state) = read_legacy_state(directory, key)? {
            if !state.workspace_id.trim().is_empty() {
                states.push((key.to_string(), state));
            }
        }
    }
    Ok(states)
}

fn read_legacy_state(
    directory: &Path,
    workspace_key: &str,
) -> Result<Option<LegacyWorkspaceState>> {
    let path = directory.join(format!("{workspace_key}.json"));
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state = match serde_json::from_str::<LegacyWorkspaceState>(&data) {
        Ok(state) if state.version == CACHE_VERSION => state,
        Ok(_) | Err(_) => return Ok(None),
    };
    Ok(Some(state))
}

fn is_workspace_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn cache_key(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn thread_key(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
}

fn merge_history_pages(existing: &[SlackMessage], page: &[SlackMessage]) -> Vec<SlackMessage> {
    // Incoming API/realtime data wins for duplicate timestamps while cached
    // messages missing from a bounded or in-flight page remain available.
    let mut messages = page.to_vec();
    messages.extend(existing.iter().cloned());
    pruned_history(messages)
}

fn merge_channel_history_pages(
    existing: &[SlackMessage],
    page: &[SlackMessage],
) -> Vec<SlackMessage> {
    channel_timeline_messages(merge_history_pages(existing, page))
}

fn merge_thread_replies_pages(
    existing: &[SlackMessage],
    page: &[SlackMessage],
) -> Vec<SlackMessage> {
    pruned_history(merge_history_pages(existing, page))
}

fn channel_timeline_messages(messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    pruned_history(
        messages
            .into_iter()
            .filter(SlackMessage::belongs_in_channel_timeline)
            .collect(),
    )
}

fn pruned_history(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages.truncate(MAX_CACHED_CHANNEL_MESSAGES);
    messages
}

#[cfg(test)]
mod tests;
